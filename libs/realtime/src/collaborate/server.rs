use crate::entities::{ClientMessage, Connect, Disconnect, Editing, RealtimeMessage, RealtimeUser};
use crate::error::{RealtimeError, StreamError};
use anyhow::Result;

use actix::{Actor, Context, Handler, ResponseFuture};
use collab::core::origin::CollabOrigin;

use collab_sync_protocol::CollabMessage;
use parking_lot::RwLock;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio_stream::wrappers::{BroadcastStream, ReceiverStream};
use tokio_stream::StreamExt;

use crate::client::ClientWSSink;
use crate::collaborate::group::CollabGroupCache;
use crate::util::channel_ext::UnboundedSenderSink;
use storage::collab::CollabStorage;

#[derive(Clone)]
pub struct CollabServer<S, U> {
  #[allow(dead_code)]
  storage: S,
  /// Keep track of all collab groups
  groups: Arc<CollabGroupCache<S, U>>,
  /// Keep track of all object ids that a user is subscribed to
  editing_collab_by_user: Arc<RwLock<HashMap<U, HashSet<Editing>>>>,
  /// Keep track of all client streams
  client_stream_by_user: Arc<RwLock<HashMap<U, CollabClientStream>>>,
}

impl<S, U> CollabServer<S, U>
where
  S: CollabStorage + Clone,
  U: RealtimeUser,
{
  pub fn new(storage: S) -> Result<Self, RealtimeError> {
    let groups = Arc::new(CollabGroupCache::new(storage.clone()));
    let edit_collab_by_user = Arc::new(RwLock::new(HashMap::new()));
    Ok(Self {
      storage,
      groups,
      editing_collab_by_user: edit_collab_by_user,
      client_stream_by_user: Default::default(),
    })
  }

  fn remove_user(&self, user: &U) {
    self.client_stream_by_user.write().remove(user);

    let editing_set = self.editing_collab_by_user.write().remove(user);
    if let Some(editing_set) = editing_set {
      tracing::info!("Remove user from group: {}", user);
      for editing in editing_set {
        remove_user_from_group(user, &self.groups, &editing);
      }
    }
  }
}

impl<S, U> Actor for CollabServer<S, U>
where
  S: 'static + Unpin,
  U: RealtimeUser + Unpin,
{
  type Context = Context<Self>;
}

impl<S, U> Handler<Connect<U>> for CollabServer<S, U>
where
  U: RealtimeUser + Unpin,
  S: CollabStorage + Unpin,
{
  type Result = Result<(), RealtimeError>;

  fn handle(&mut self, new_conn: Connect<U>, _ctx: &mut Context<Self>) -> Self::Result {
    tracing::trace!("[💭Server]: new connection => {} ", new_conn.user);
    // Remove the user from the group if the user is already connected
    self.remove_user(&new_conn.user);

    let stream = CollabClientStream::new(ClientWSSink(new_conn.socket));
    self
      .client_stream_by_user
      .write()
      .insert(new_conn.user, stream);
    Ok(())
  }
}

impl<S, U> Handler<Disconnect<U>> for CollabServer<S, U>
where
  U: RealtimeUser + Unpin,
  S: CollabStorage + Unpin,
{
  type Result = Result<(), RealtimeError>;
  fn handle(&mut self, msg: Disconnect<U>, _: &mut Context<Self>) -> Self::Result {
    tracing::trace!("[💭Server]: disconnect => {}", msg.user);
    self.remove_user(&msg.user);
    Ok(())
  }
}

impl<S, U> Handler<ClientMessage<U>> for CollabServer<S, U>
where
  U: RealtimeUser + Unpin,
  S: CollabStorage + Unpin,
{
  type Result = ResponseFuture<Result<(), RealtimeError>>;

  fn handle(&mut self, client_msg: ClientMessage<U>, _ctx: &mut Context<Self>) -> Self::Result {
    let client_streams = self.client_stream_by_user.clone();
    let groups = self.groups.clone();
    let edit_collab_by_user = self.editing_collab_by_user.clone();

    Box::pin(async move {
      subscribe_collab_group_change_if_need(
        &client_msg,
        &groups,
        &edit_collab_by_user,
        &client_streams,
      )
      .await?;
      forward_message_to_collab_group(&client_msg, &client_streams).await;
      Ok(())
    })
  }
}

async fn forward_message_to_collab_group<U>(
  client_msg: &ClientMessage<U>,
  client_streams: &Arc<RwLock<HashMap<U, CollabClientStream>>>,
) where
  U: RealtimeUser,
{
  if let Some(client_stream) = client_streams.read().get(&client_msg.user) {
    tracing::trace!(
      "[💭Server]: receives: user:{} message: [oid:{}|msg_id:{:?}]",
      client_msg.user,
      client_msg.content.object_id(),
      client_msg.content.msg_id()
    );
    match client_stream
      .stream_tx
      .send(Ok(RealtimeMessage::from(client_msg.clone())))
    {
      Ok(_) => {},
      Err(e) => {
        tracing::error!("🔴send error: {}", e)
      },
    }
  }
}

async fn subscribe_collab_group_change_if_need<U, S>(
  client_msg: &ClientMessage<U>,
  groups: &Arc<CollabGroupCache<S, U>>,
  edit_collab_by_user: &Arc<RwLock<HashMap<U, HashSet<Editing>>>>,
  client_streams: &Arc<RwLock<HashMap<U, CollabClientStream>>>,
) -> Result<(), RealtimeError>
where
  U: RealtimeUser,
  S: CollabStorage,
{
  let object_id = client_msg.content.object_id();
  if !groups.read().contains_key(object_id) {
    // When create a group, the message must be the init sync message.
    match &client_msg.content {
      CollabMessage::ClientInit(client_init) => {
        let uid = client_init
          .origin
          .client_user_id()
          .ok_or(RealtimeError::UnexpectedData("The client user id is empty"))?;
        groups
          .create_group(
            uid,
            &client_init.workspace_id,
            object_id,
            client_init.collab_type.clone(),
          )
          .await;
      },
      _ => {
        return Err(RealtimeError::UnexpectedData(
          "The first message must be init sync message",
        ));
      },
    }
  }

  let origin = match client_msg.content.origin() {
    None => {
      tracing::error!("🔴The origin from client message is empty");
      &CollabOrigin::Empty
    },
    Some(client) => client,
  };

  // If the client's stream is already subscribed to the collab group, return.
  if groups
    .read()
    .get(object_id)
    .map(|group| group.subscribers.read().get(&client_msg.user).is_some())
    .unwrap_or(false)
  {
    return Ok(());
  }

  match client_streams.write().get_mut(&client_msg.user) {
    None => tracing::error!("🔴The client stream is not found"),
    Some(client_stream) => {
      if let Some(collab_group) = groups.write().get_mut(object_id) {
        collab_group
          .subscribers
          .write()
          .entry(client_msg.user.clone())
          .or_insert_with(|| {
            tracing::trace!(
              "[💭Server]: {} subscribe group:{}",
              client_msg.user,
              client_msg.content.object_id()
            );

            edit_collab_by_user
              .write()
              .entry(client_msg.user.clone())
              .or_default()
              .insert(Editing {
                object_id: object_id.to_string(),
                origin: origin.clone(),
              });

            let (sink, stream) = client_stream
              .client_channel::<CollabMessage, _, _>(
                object_id,
                move |object_id, msg| msg.object_id() == object_id,
                move |object_id, msg| msg.object_id == object_id,
              )
              .unwrap();

            collab_group
              .broadcast
              .subscribe(origin.clone(), sink, stream)
          });
      }
    },
  }

  Ok(())
}

/// Remove the user from the group and remove the group from the cache if the group is empty.
fn remove_user_from_group<S, U>(user: &U, groups: &Arc<CollabGroupCache<S, U>>, editing: &Editing)
where
  S: CollabStorage,
  U: RealtimeUser,
{
  let mut groups_write_guard = groups.write();

  let should_remove_group = groups_write_guard.get_mut(&editing.object_id).map(|group| {
    tracing::info!("Remove subscriber: {}", editing.origin);
    group.subscribers.write().remove(user);
    let should_remove = group.is_empty();
    if should_remove {
      group.flush_collab();
    }
    should_remove
  });

  // If the group is empty, remove it from the cache
  if should_remove_group.unwrap_or(false) {
    tracing::debug!("Remove group: {}", editing.object_id);
    groups_write_guard.remove(&editing.object_id);
  }
}

impl<S, U> actix::Supervised for CollabServer<S, U>
where
  S: 'static + Unpin,
  U: RealtimeUser + Unpin,
{
  fn restarting(&mut self, _ctx: &mut Context<CollabServer<S, U>>) {
    tracing::warn!("restarting");
  }
}

impl TryFrom<RealtimeMessage> for CollabMessage {
  type Error = StreamError;

  fn try_from(value: RealtimeMessage) -> Result<Self, Self::Error> {
    CollabMessage::from_vec(&value.payload).map_err(|e| StreamError::Internal(e.to_string()))
  }
}

pub struct CollabClientStream {
  ws_sink: ClientWSSink,
  /// Used to receive messages from the collab server
  pub(crate) stream_tx: tokio::sync::broadcast::Sender<Result<RealtimeMessage, StreamError>>,
}

impl CollabClientStream {
  pub fn new(sink: ClientWSSink) -> Self {
    // When receive a new connection, create a new [ClientStream] that holds the connection's websocket
    let (stream_tx, _) = tokio::sync::broadcast::channel(1000);
    Self {
      ws_sink: sink,
      stream_tx,
    }
  }

  /// Returns a [UnboundedSenderSink] and a [ReceiverStream] for the object_id.
  #[allow(clippy::type_complexity)]
  pub fn client_channel<T, F1, F2>(
    &mut self,
    object_id: &str,
    sink_filter: F1,
    stream_filter: F2,
  ) -> Option<(
    UnboundedSenderSink<T>,
    ReceiverStream<Result<T, StreamError>>,
  )>
  where
    T:
      TryFrom<RealtimeMessage, Error = StreamError> + Into<RealtimeMessage> + Send + Sync + 'static,
    F1: Fn(&str, &T) -> bool + Send + Sync + 'static,
    F2: Fn(&str, &RealtimeMessage) -> bool + Send + Sync + 'static,
  {
    let client_ws_sink = self.ws_sink.clone();
    let mut stream_rx = BroadcastStream::new(self.stream_tx.subscribe());
    let cloned_object_id = object_id.to_string();

    // Send the message to the connected websocket client
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<T>();
    tokio::spawn(async move {
      while let Some(msg) = rx.recv().await {
        if sink_filter(&cloned_object_id, &msg) {
          client_ws_sink.do_send(msg.into());
        }
      }
    });
    let client_forward_sink = UnboundedSenderSink::<T>::new(tx);

    // forward the message to the stream that can be subscribed by the broadcast group, which will
    // send the messages to all connected clients using the client_forward_sink
    let cloned_object_id = object_id.to_string();
    let (tx, rx) = tokio::sync::mpsc::channel(100);
    tokio::spawn(async move {
      while let Some(Ok(Ok(msg))) = stream_rx.next().await {
        if stream_filter(&cloned_object_id, &msg) {
          let _ = tx.send(T::try_from(msg)).await;
        }
      }
    });
    let client_forward_stream = ReceiverStream::new(rx);

    // When broadcast group write a message to the client_forward_sink, the message will be forwarded
    // to the client's websocket sink, which will then send the message to the connected client
    //
    // When receiving a message from the client_forward_stream, it will send the message to the broadcast
    // group. The message will be broadcast to all connected clients.
    Some((client_forward_sink, client_forward_stream))
  }
}