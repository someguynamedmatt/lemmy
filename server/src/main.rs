extern crate lemmy_server;
#[macro_use]
extern crate diesel_migrations;

use actix_web::*;
use actix::prelude::*;
use lemmy_server::routes::{api, federation, feeds, index, nodeinfo, webfinger, websocket};
use lemmy_server::settings::Settings;
use lemmy_server::websocket::server::*;
use std::io;

embed_migrations!();

#[actix_rt::main]
async fn main() -> io::Result<()> {
  env_logger::init();

  // Set up the Chatserver's shared state
  let cs_state_web = web::Data::new(ChatSharedState::init());
  let cs_state_web_two = cs_state_web.clone();
  let server = SyncArbiter::start(2, move || ChatServer::init(cs_state_web.clone()));

  let settings = cs_state_web_two.settings.lock().unwrap().to_owned();

  // Run the migrations from code
  // let conn = pool.get().unwrap();
  let conn = cs_state_web_two.pool.lock().unwrap().get().unwrap();
  embedded_migrations::run(&conn).unwrap();


  // Set up websocket server
  // let server = ChatServer::init(cs_state).start();

  // let cs_state_web_two = cs_state_web.clone();

  println!(
    "Starting http server at {}:{}",
    settings.bind, settings.port
  );

  // Create Http server with websocket support
  HttpServer::new(move || {
    let settings = cs_state_web_two.settings.lock().unwrap().to_owned();
    App::new()
      .wrap(middleware::Logger::default())
      // .app_data(cs_state_web_two.clone())
      .app_data(cs_state_web_two.clone())
      .data(server.clone())
      // The routes
      .configure(api::config)
      .configure(federation::config)
      .configure(feeds::config)
      .configure(index::config)
      .configure(nodeinfo::config)
      .configure(webfinger::config)
      .configure(websocket::config)
      // static files
      .service(actix_files::Files::new(
        "/static",
        settings.front_end_dir.to_owned(),
      ))
      .service(actix_files::Files::new(
        "/docs",
        settings.front_end_dir.to_owned() + "/documentation",
      ))
  })
  .bind((settings.bind, settings.port))?
  .run()
  .await
}
