extern crate lemmy_server;
#[macro_use]
extern crate diesel_migrations;

use actix::prelude::*;
use actix_web::*;
use lemmy_server::routes::{api, federation, feeds, index, nodeinfo, webfinger, websocket};
use lemmy_server::settings::Settings;
use lemmy_server::websocket::server::*;
use lemmy_server::db::establish_unpooled_connection;
use lemmy_server::DbHandle;
use std::io;

embed_migrations!();


#[actix_rt::main]
async fn main() -> io::Result<()> {
  env_logger::init();
  let settings = Settings::get();

  let db_handle = DbHandle::start(&settings).unwrap();

  // Run the migrations from code
  embedded_migrations::run(&establish_unpooled_connection()).unwrap();

  // Set up websocket server
  let server = ChatServer::startup(db_handle.clone()).start();

  println!(
    "Starting http server at {}:{}",
    settings.bind, settings.port
  );

  // Create Http server with websocket support
  HttpServer::new(move || {
    let settings = Settings::get();
    App::new()
      .wrap(middleware::Logger::default())
      .data(db_handle.clone())
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
        settings.front_end_dir + "/documentation",
      ))
  })
  .bind((settings.bind, settings.port))?
  .run()
  .await
}
