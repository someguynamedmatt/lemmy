//! `ChatServer` is an actor. It maintains list of connection client session.
//! And manages available rooms. Peers send messages to other peers in same
//! room through `ChatServer`.

use actix::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool, PooledConnection};
use diesel::PgConnection;
use failure::Error;
use log::{error, info, warn};
use rand::{Rng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::SystemTime;
use std::sync::Mutex;
use std::sync::Arc;
use strum::IntoEnumIterator;
use actix_web::web;

use crate::api::comment::*;
use crate::api::community::*;
use crate::api::post::*;
use crate::api::site::*;
use crate::api::user::*;
use crate::api::*;
use crate::websocket::UserOperation;
use crate::Settings;

type ConnectionId = usize;
type PostId = i32;
type CommunityId = i32;
type UserId = i32;
type IPAddr = String;

/// Chat server sends this messages to session
#[derive(Message)]
#[rtype(result = "()")]
pub struct WSMessage(pub String);

/// Message for chat server communications

/// New chat session is created
#[derive(Message)]
#[rtype(usize)]
pub struct Connect {
  pub addr: Recipient<WSMessage>,
  pub ip: IPAddr,
}

/// Session is disconnected
#[derive(Message)]
#[rtype(result = "()")]
pub struct Disconnect {
  pub id: ConnectionId,
  pub ip: IPAddr,
}

#[derive(Serialize, Deserialize, Message)]
#[rtype(String)]
pub struct StandardMessage {
  /// Id of the client session
  pub id: ConnectionId,
  /// Peer message
  pub msg: String,
}

#[derive(Debug)]
pub struct RateLimitBucket {
  last_checked: SystemTime,
  allowance: f64,
}

pub struct SessionInfo {
  pub addr: Recipient<WSMessage>,
  pub ip: IPAddr,
}

#[derive(Eq, PartialEq, Hash, Debug, EnumIter, Copy, Clone)]
pub enum RateLimitType {
  Message,
  Register,
  Post,
}

pub struct ChatSharedState {
  /// The Settings
  pub settings: Mutex<Settings>,

  /// Connection pool
  pub pool: Mutex<Pool<ConnectionManager<PgConnection>>>,

  /// A map from generated random ID to session addr
  sessions: Mutex<HashMap<ConnectionId, SessionInfo>>,

  /// A map from post_id to set of connectionIDs
  post_rooms: Mutex<HashMap<PostId, HashSet<ConnectionId>>>,

  /// A map from community to set of connectionIDs
  community_rooms: Mutex<HashMap<CommunityId, HashSet<ConnectionId>>>,

  /// A map from user id to its connection ID for joined users. Remember a user can have multiple
  /// sessions (IE clients)
  user_rooms: Mutex<HashMap<UserId, HashSet<ConnectionId>>>,

  /// Rate limiting based on rate type and IP addr
  rate_limit_buckets: Mutex<HashMap<RateLimitType, HashMap<IPAddr, RateLimitBucket>>>,
}

impl ChatSharedState {  
  pub fn init() -> Self {

    // Setup Settings
    let settings = match Settings::init() {
      Ok(c) => c,
      Err(e) => panic!("{}", e),
    };

    // Set up the r2d2 connection pool
    let manager = ConnectionManager::<PgConnection>::new(&settings.get_database_url());
    let pool = Pool::builder()
      .max_size(settings.database.pool_size)
      .build(manager)
      .unwrap_or_else(|_| panic!("Error connecting to {}", settings.get_database_url()));

    ChatSharedState {
      settings: Mutex::new(settings),
      pool: Mutex::new(pool),
      sessions: Mutex::new(HashMap::new()),
      rate_limit_buckets: Mutex::new(HashMap::new()),
      post_rooms: Mutex::new(HashMap::new()),
      community_rooms: Mutex::new(HashMap::new()),
      user_rooms: Mutex::new(HashMap::new()),
    }
  }
}


/// `ChatServer` manages chat rooms and responsible for coordinating chat
/// session.
pub struct ChatServer { 
  state: web::Data<ChatSharedState>,
}

impl ChatServer {
  pub fn init(state: web::Data<ChatSharedState>) -> Self {
    ChatServer {
      state,
    }
  }

  fn join_community_room(&self, community_id: CommunityId, id: ConnectionId) {
    // remove session from all rooms
    for sessions in self.state.community_rooms.lock().unwrap().values_mut() {
      sessions.remove(&id);
    }

    // Also leave all post rooms
    // This avoids double messages
    for sessions in self.state.post_rooms.lock().unwrap().values_mut() {
      sessions.remove(&id);
    }

    // If the room doesn't exist yet
    if self.state.community_rooms.lock().unwrap().get_mut(&community_id).is_none() {
      self.state.community_rooms.lock().unwrap().insert(community_id, HashSet::new());
    }

    self
      .state
      .community_rooms
      .lock()
      .unwrap()
      .get_mut(&community_id)
      .unwrap()
      .insert(id);
  }

  fn join_post_room(&self, post_id: PostId, id: ConnectionId) {
    // remove session from all rooms
    for sessions in self.state.post_rooms.lock().unwrap().values_mut() {
      sessions.remove(&id);
    }

    // Also leave all communities
    // This avoids double messages
    for sessions in self.state.community_rooms.lock().unwrap().values_mut() {
      sessions.remove(&id);
    }

    // If the room doesn't exist yet
    if self.state.post_rooms.lock().unwrap().get_mut(&post_id).is_none() {
      self.state.post_rooms.lock().unwrap().insert(post_id, HashSet::new());
    }

    self.state.post_rooms.lock().unwrap().get_mut(&post_id).unwrap().insert(id);
  }

  fn join_user_room(&self, user_id: UserId, id: ConnectionId) {
    // remove session from all rooms
    for sessions in self.state.user_rooms.lock().unwrap().values_mut() {
      sessions.remove(&id);
    }

    // If the room doesn't exist yet
    if self.state.user_rooms.lock().unwrap().get_mut(&user_id).is_none() {
      self.state.user_rooms.lock().unwrap().insert(user_id, HashSet::new());
    }

    self.state.user_rooms.lock().unwrap().get_mut(&user_id).unwrap().insert(id);
  }

  fn send_post_room_message(&self, post_id: PostId, message: &str, skip_id: ConnectionId) {
    if let Some(sessions) = self.state.post_rooms.lock().unwrap().get(&post_id) {
      for id in sessions {
        if *id != skip_id {
          if let Some(info) = self.state.sessions.lock().unwrap().get(id) {
            let _ = info.addr.do_send(WSMessage(message.to_owned()));
          }
        }
      }
    }
  }

  fn send_community_room_message(
    &self,
    community_id: CommunityId,
    message: &str,
    skip_id: ConnectionId,
  ) {
    if let Some(sessions) = self.state.community_rooms.lock().unwrap().get(&community_id) {
      for id in sessions {
        if *id != skip_id {
          if let Some(info) = self.state.sessions.lock().unwrap().get(id) {
            let _ = info.addr.do_send(WSMessage(message.to_owned()));
          }
        }
      }
    }
  }

  fn send_user_room_message(&self, user_id: UserId, message: &str, skip_id: ConnectionId) {
    if let Some(sessions) = self.state.user_rooms.lock().unwrap().get(&user_id) {
      for id in sessions {
        if *id != skip_id {
          if let Some(info) = self.state.sessions.lock().unwrap().get(id) {
            let _ = info.addr.do_send(WSMessage(message.to_owned()));
          }
        }
      }
    }
  }

  fn send_all_message(&self, message: &str, skip_id: ConnectionId) {
    for id in self.state.sessions.lock().unwrap().keys() {
      if *id != skip_id {
        if let Some(info) = self.state.sessions.lock().unwrap().get(id) {
          let _ = info.addr.do_send(WSMessage(message.to_owned()));
        }
      }
    }
  }

  fn comment_sends(
    &self,
    user_operation: UserOperation,
    comment: CommentResponse,
    id: ConnectionId,
  ) -> Result<String, Error> {
    let mut comment_reply_sent = comment.clone();
    comment_reply_sent.comment.my_vote = None;
    comment_reply_sent.comment.user_id = None;

    // For the post room ones, and the directs back to the user
    // strip out the recipient_ids, so that
    // users don't get double notifs
    let mut comment_user_sent = comment.clone();
    comment_user_sent.recipient_ids = Vec::new();

    let mut comment_post_sent = comment_reply_sent.clone();
    comment_post_sent.recipient_ids = Vec::new();

    let comment_reply_sent_str = to_json_string(&user_operation, &comment_reply_sent)?;
    let comment_post_sent_str = to_json_string(&user_operation, &comment_post_sent)?;
    let comment_user_sent_str = to_json_string(&user_operation, &comment_user_sent)?;

    // Send it to the post room
    self.send_post_room_message(comment.comment.post_id, &comment_post_sent_str, id);

    // Send it to the recipient(s) including the mentioned users
    for recipient_id in comment_reply_sent.recipient_ids {
      self.send_user_room_message(recipient_id, &comment_reply_sent_str, id);
    }

    // Send it to the community too
    self.send_community_room_message(0, &comment_post_sent_str, id);
    self.send_community_room_message(comment.comment.community_id, &comment_post_sent_str, id);

    Ok(comment_user_sent_str)
  }

  fn post_sends(
    &self,
    user_operation: UserOperation,
    post: PostResponse,
    id: ConnectionId,
  ) -> Result<String, Error> {
    let community_id = post.post.community_id;

    // Don't send my data with it
    let mut post_sent = post.clone();
    post_sent.post.my_vote = None;
    post_sent.post.user_id = None;
    let post_sent_str = to_json_string(&user_operation, &post_sent)?;

    // Send it to /c/all and that community
    self.send_community_room_message(0, &post_sent_str, id);
    self.send_community_room_message(community_id, &post_sent_str, id);

    // Send it to the post room
    self.send_post_room_message(post_sent.post.id, &post_sent_str, id);

    to_json_string(&user_operation, post)
  }

  fn check_rate_limit_register(&self, id: usize, check_only: bool) -> Result<(), Error> {
    self.check_rate_limit_full(
      RateLimitType::Register,
      id,
      self.state.settings.lock().unwrap().rate_limit.register,
      self.state.settings.lock().unwrap().rate_limit.register_per_second,
      check_only,
    )
  }

  fn check_rate_limit_post(&self, id: usize, check_only: bool) -> Result<(), Error> {
    self.check_rate_limit_full(
      RateLimitType::Post,
      id,
      self.state.settings.lock().unwrap().rate_limit.post,
      self.state.settings.lock().unwrap().rate_limit.post_per_second,
      check_only,
    )
  }

  fn check_rate_limit_message(&self, id: usize, check_only: bool) -> Result<(), Error> {
    self.check_rate_limit_full(
      RateLimitType::Message,
      id,
      self.state.settings.lock().unwrap().rate_limit.message,
      self.state.settings.lock().unwrap().rate_limit.message_per_second,
      check_only,
    )
  }

  #[allow(clippy::float_cmp)]
  fn check_rate_limit_full(
    &self,
    type_: RateLimitType,
    id: usize,
    rate: i32,
    per: i32,
    check_only: bool,
  ) -> Result<(), Error> {
    if let Some(info) = self.state.sessions.lock().unwrap().get(&id) {
      if let Some(bucket) = self.state.rate_limit_buckets.lock().unwrap().get_mut(&type_) {
        if let Some(rate_limit) = bucket.get_mut(&info.ip) {
          let current = SystemTime::now();
          let time_passed = current.duration_since(rate_limit.last_checked)?.as_secs() as f64;

          // The initial value
          if rate_limit.allowance == -2f64 {
            rate_limit.allowance = rate as f64;
          };

          rate_limit.last_checked = current;
          rate_limit.allowance += time_passed * (rate as f64 / per as f64);
          if !check_only && rate_limit.allowance > rate as f64 {
            rate_limit.allowance = rate as f64;
          }

          if rate_limit.allowance < 1.0 {
            warn!(
              "Rate limited IP: {}, time_passed: {}, allowance: {}",
              &info.ip, time_passed, rate_limit.allowance
            );
            Err(
              APIError {
                message: format!("Too many requests. {} per {} seconds", rate, per),
              }
              .into(),
            )
          } else {
            if !check_only {
              rate_limit.allowance -= 1.0;
            }
            Ok(())
          }
        } else {
          Ok(())
        }
      } else {
        Ok(())
      }
    } else {
      Ok(())
    }
  }
}

/// Make actor from `ChatServer`
impl Actor for ChatServer {
  /// We are going to use simple Context, we just need ability to communicate
  /// with other actors.
  type Context = SyncContext<Self>;
}

/// Handler for Connect message.
///
/// Register new session and assign unique id to this session
impl Handler<Connect> for ChatServer {
  type Result = usize;

  fn handle(&mut self, msg: Connect, _ctx: &mut SyncContext<Self>) -> Self::Result {
    // register session with random id
    let id = rand::thread_rng().gen::<usize>();
    info!("{} joined", &msg.ip);

    self.state.sessions.lock().unwrap().insert(
      id,
      SessionInfo {
        addr: msg.addr,
        ip: msg.ip.to_owned(),
      },
    );

    for rate_limit_type in RateLimitType::iter() {
      if self.state.rate_limit_buckets.lock().unwrap().get(&rate_limit_type).is_none() {
        self
          .state
          .rate_limit_buckets
          .lock()
          .unwrap()
          .insert(rate_limit_type, HashMap::new());
      }

      if let Some(bucket) = self.state.rate_limit_buckets.lock().unwrap().get_mut(&rate_limit_type) {
        if bucket.get(&msg.ip).is_none() {
          bucket.insert(
            msg.ip.to_owned(),
            RateLimitBucket {
              last_checked: SystemTime::now(),
              allowance: -2f64,
            },
          );
        }
      }
    }

    id
  }
}

/// Handler for Disconnect message.
impl Handler<Disconnect> for ChatServer {
  type Result = ();

  fn handle(&mut self, msg: Disconnect, _: &mut SyncContext<Self>) {
    // Remove connections from sessions and all 3 scopes
    if self.state.sessions.lock().unwrap().remove(&msg.id).is_some() {
      for sessions in self.state.user_rooms.lock().unwrap().values_mut() {
        sessions.remove(&msg.id);
      }

      for sessions in self.state.post_rooms.lock().unwrap().values_mut() {
        sessions.remove(&msg.id);
      }

      for sessions in self.state.community_rooms.lock().unwrap().values_mut() {
        sessions.remove(&msg.id);
      }
    }
  }
}

/// Handler for Message message.
impl Handler<StandardMessage> for ChatServer {
  type Result = MessageResult<StandardMessage>;

  fn handle(&mut self, msg: StandardMessage, _: &mut SyncContext<Self>) -> Self::Result {
    match parse_json_message(self, msg) {
      Ok(m) => {
        info!("Message Sent: {}", m);
        MessageResult(m)
      }
      Err(e) => {
        error!("Error during message handling {}", e);
        MessageResult(e.to_string())
      }
    }
  }
}

#[derive(Serialize)]
struct WebsocketResponse<T> {
  op: String,
  data: T,
}

fn to_json_string<T>(op: &UserOperation, data: T) -> Result<String, Error>
where
  T: Serialize,
{
  let response = WebsocketResponse {
    op: op.to_string(),
    data,
  };
  Ok(serde_json::to_string(&response)?)
}

fn do_user_operation<'a, Data, Response>(
  op: UserOperation,
  data: &str,
  conn: &PooledConnection<ConnectionManager<PgConnection>>,
  settings: &Settings,
) -> Result<String, Error>
where
  for<'de> Data: Deserialize<'de> + 'a,
  Response: Serialize,
  Oper<Data>: Perform<Response>,
{
  let parsed_data: Data = serde_json::from_str(data)?;
  let res = Oper::new(parsed_data).perform(&conn, &settings)?;
  to_json_string(&op, &res)
}

fn parse_json_message(chat: &mut ChatServer, msg: StandardMessage) -> Result<String, Error> {
  let json: Value = serde_json::from_str(&msg.msg)?;
  let data = &json["data"].to_string();
  let op = &json["op"].as_str().ok_or(APIError {
    message: "Unknown op type".to_string(),
  })?;

  let conn = chat.state.pool.lock().unwrap().get()?;
  let settings = chat.state.settings.lock().unwrap();

  let user_operation: UserOperation = UserOperation::from_str(&op)?;

  // TODO: none of the chat messages are going to work if stuff is submitted via http api,
  //       need to move that handling elsewhere

  // A DDOS check
  chat.check_rate_limit_message(msg.id, false)?;

  match user_operation {
    UserOperation::Login => do_user_operation::<Login, LoginResponse>(user_operation, data, &conn, &settings),
    UserOperation::Register => {
      chat.check_rate_limit_register(msg.id, true)?;
      let register: Register = serde_json::from_str(data)?;
      let res = Oper::new(register).perform(&conn, &settings)?;
      chat.check_rate_limit_register(msg.id, false)?;
      to_json_string(&user_operation, &res)
    }
    UserOperation::GetUserDetails => {
      do_user_operation::<GetUserDetails, GetUserDetailsResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::SaveUserSettings => {
      do_user_operation::<SaveUserSettings, LoginResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::AddAdmin => {
      let add_admin: AddAdmin = serde_json::from_str(data)?;
      let res = Oper::new(add_admin).perform(&conn, &settings)?;
      let res_str = to_json_string(&user_operation, &res)?;
      chat.send_all_message(&res_str, msg.id);
      Ok(res_str)
    }
    UserOperation::BanUser => {
      let ban_user: BanUser = serde_json::from_str(data)?;
      let res = Oper::new(ban_user).perform(&conn, &settings)?;
      let res_str = to_json_string(&user_operation, &res)?;
      chat.send_all_message(&res_str, msg.id);
      Ok(res_str)
    }
    UserOperation::GetReplies => {
      do_user_operation::<GetReplies, GetRepliesResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::GetUserMentions => {
      do_user_operation::<GetUserMentions, GetUserMentionsResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::EditUserMention => {
      do_user_operation::<EditUserMention, UserMentionResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::MarkAllAsRead => {
      do_user_operation::<MarkAllAsRead, GetRepliesResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::GetCommunity => {
      let get_community: GetCommunity = serde_json::from_str(data)?;
      let mut res = Oper::new(get_community).perform(&conn, &settings)?;
      let community_id = res.community.id;

      chat.join_community_room(community_id, msg.id);

      res.online = if let Some(community_users) = chat.state.community_rooms.lock().unwrap().get(&community_id) {
        community_users.len()
      } else {
        0
      };

      to_json_string(&user_operation, &res)
    }
    UserOperation::ListCommunities => {
      do_user_operation::<ListCommunities, ListCommunitiesResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::CreateCommunity => {
      chat.check_rate_limit_register(msg.id, true)?;
      let create_community: CreateCommunity = serde_json::from_str(data)?;
      let res = Oper::new(create_community).perform(&conn, &settings)?;
      chat.check_rate_limit_register(msg.id, false)?;
      to_json_string(&user_operation, &res)
    }
    UserOperation::EditCommunity => {
      let edit_community: EditCommunity = serde_json::from_str(data)?;
      let res = Oper::new(edit_community).perform(&conn, &settings)?;
      let mut community_sent: CommunityResponse = res.clone();
      community_sent.community.user_id = None;
      community_sent.community.subscribed = None;
      let community_sent_str = to_json_string(&user_operation, &community_sent)?;
      chat.send_community_room_message(community_sent.community.id, &community_sent_str, msg.id);
      to_json_string(&user_operation, &res)
    }
    UserOperation::FollowCommunity => {
      do_user_operation::<FollowCommunity, CommunityResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::GetFollowedCommunities => do_user_operation::<
      GetFollowedCommunities,
      GetFollowedCommunitiesResponse,
    >(user_operation, data, &conn, &settings),
    UserOperation::BanFromCommunity => {
      let ban_from_community: BanFromCommunity = serde_json::from_str(data)?;
      let community_id = ban_from_community.community_id;
      let res = Oper::new(ban_from_community).perform(&conn, &settings)?;
      let res_str = to_json_string(&user_operation, &res)?;
      chat.send_community_room_message(community_id, &res_str, msg.id);
      Ok(res_str)
    }
    UserOperation::AddModToCommunity => {
      let mod_add_to_community: AddModToCommunity = serde_json::from_str(data)?;
      let community_id = mod_add_to_community.community_id;
      let res = Oper::new(mod_add_to_community).perform(&conn, &settings)?;
      let res_str = to_json_string(&user_operation, &res)?;
      chat.send_community_room_message(community_id, &res_str, msg.id);
      Ok(res_str)
    }
    UserOperation::ListCategories => {
      do_user_operation::<ListCategories, ListCategoriesResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::GetPost => {
      let get_post: GetPost = serde_json::from_str(data)?;
      let post_id = get_post.id;
      chat.join_post_room(post_id, msg.id);
      let mut res = Oper::new(get_post).perform(&conn, &settings)?;

      res.online = if let Some(post_users) = chat.state.post_rooms.lock().unwrap().get(&post_id) {
        post_users.len()
      } else {
        0
      };

      to_json_string(&user_operation, &res)
    }
    UserOperation::GetPosts => {
      let get_posts: GetPosts = serde_json::from_str(data)?;
      if get_posts.community_id.is_none() {
        // 0 is the "all" community
        chat.join_community_room(0, msg.id);
      }
      let res = Oper::new(get_posts).perform(&conn, &settings)?;
      to_json_string(&user_operation, &res)
    }
    UserOperation::GetComments => {
      let get_comments: GetComments = serde_json::from_str(data)?;
      if get_comments.community_id.is_none() {
        // 0 is the "all" community
        chat.join_community_room(0, msg.id);
      }
      let res = Oper::new(get_comments).perform(&conn, &settings)?;
      to_json_string(&user_operation, &res)
    }
    UserOperation::CreatePost => {
      chat.check_rate_limit_post(msg.id, true)?;
      let create_post: CreatePost = serde_json::from_str(data)?;
      let res = Oper::new(create_post).perform(&conn, &settings)?;
      chat.check_rate_limit_post(msg.id, false)?;

      chat.post_sends(UserOperation::CreatePost, res, msg.id)
    }
    UserOperation::CreatePostLike => {
      let create_post_like: CreatePostLike = serde_json::from_str(data)?;
      let res = Oper::new(create_post_like).perform(&conn, &settings)?;

      chat.post_sends(UserOperation::CreatePostLike, res, msg.id)
    }
    UserOperation::EditPost => {
      let edit_post: EditPost = serde_json::from_str(data)?;
      let res = Oper::new(edit_post).perform(&conn, &settings)?;

      chat.post_sends(UserOperation::EditPost, res, msg.id)
    }
    UserOperation::SavePost => {
      do_user_operation::<SavePost, PostResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::CreateComment => {
      let create_comment: CreateComment = serde_json::from_str(data)?;
      let res = Oper::new(create_comment).perform(&conn, &settings)?;

      chat.comment_sends(UserOperation::CreateComment, res, msg.id)
    }
    UserOperation::EditComment => {
      let edit_comment: EditComment = serde_json::from_str(data)?;
      let res = Oper::new(edit_comment).perform(&conn, &settings)?;

      chat.comment_sends(UserOperation::EditComment, res, msg.id)
    }
    UserOperation::SaveComment => {
      do_user_operation::<SaveComment, CommentResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::CreateCommentLike => {
      let create_comment_like: CreateCommentLike = serde_json::from_str(data)?;
      let res = Oper::new(create_comment_like).perform(&conn, &settings)?;

      chat.comment_sends(UserOperation::CreateCommentLike, res, msg.id)
    }
    UserOperation::GetModlog => {
      do_user_operation::<GetModlog, GetModlogResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::CreateSite => {
      do_user_operation::<CreateSite, SiteResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::EditSite => {
      let edit_site: EditSite = serde_json::from_str(data)?;
      let res = Oper::new(edit_site).perform(&conn, &settings)?;
      let res_str = to_json_string(&user_operation, &res)?;
      chat.send_all_message(&res_str, msg.id);
      Ok(res_str)
    }
    UserOperation::GetSite => {
      let get_site: GetSite = serde_json::from_str(data)?;
      let mut res = Oper::new(get_site).perform(&conn, &settings)?;
      res.online = chat.state.sessions.lock().unwrap().len();
      to_json_string(&user_operation, &res)
    }
    UserOperation::GetSiteConfig => {
      let get_site_config: GetSiteConfig = serde_json::from_str(data)?;
      let res = Oper::new(get_site_config).perform(&conn, &settings)?;
      to_json_string(&user_operation, &res)
    }
    UserOperation::SaveSiteConfig => {
      let save_site_config: SaveSiteConfig = serde_json::from_str(data)?;
      let res = Oper::new(save_site_config).perform(&conn, &settings)?;
      let mut new_settings = chat.state.settings.lock().unwrap();
      *new_settings = match Settings::init() {
        Ok(c) => c,
        Err(e) => panic!("{}", e),
      };
      to_json_string(&user_operation, &res)
    }
    UserOperation::Search => {
      do_user_operation::<Search, SearchResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::TransferCommunity => {
      do_user_operation::<TransferCommunity, GetCommunityResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::TransferSite => {
      do_user_operation::<TransferSite, GetSiteResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::DeleteAccount => {
      do_user_operation::<DeleteAccount, LoginResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::PasswordReset => {
      do_user_operation::<PasswordReset, PasswordResetResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::PasswordChange => {
      do_user_operation::<PasswordChange, LoginResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::CreatePrivateMessage => {
      let create_private_message: CreatePrivateMessage = serde_json::from_str(data)?;
      let recipient_id = create_private_message.recipient_id;
      let res = Oper::new(create_private_message).perform(&conn, &settings)?;
      let res_str = to_json_string(&user_operation, &res)?;

      chat.send_user_room_message(recipient_id, &res_str, msg.id);
      Ok(res_str)
    }
    UserOperation::EditPrivateMessage => {
      do_user_operation::<EditPrivateMessage, PrivateMessageResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::GetPrivateMessages => {
      do_user_operation::<GetPrivateMessages, PrivateMessagesResponse>(user_operation, data, &conn, &settings)
    }
    UserOperation::UserJoin => {
      let user_join: UserJoin = serde_json::from_str(data)?;
      let res = Oper::new(user_join).perform(&conn, &settings)?;
      chat.join_user_room(res.user_id, msg.id);
      to_json_string(&user_operation, &res)
    }
  }
}
