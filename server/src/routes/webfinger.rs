use crate::db::community::Community;
use crate::websocket::server::ChatSharedState;
use crate::Settings;
use actix_web::web;
use actix_web::web::Query;
use actix_web::HttpResponse;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::PgConnection;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct Params {
  resource: String,
}

pub fn config(
  cfg: &mut web::ServiceConfig,
) {

  // TODO couldn't get this to pass the app state
  // if state.settings.lock().unwrap().federation_enabled {
    cfg.route(
      ".well-known/webfinger",
      web::get().to(get_webfinger_response),
    );
  // }
}

/// Responds to webfinger requests of the following format. There isn't any real documentation for
/// this, but it described in this blog post:
/// https://mastodon.social/.well-known/webfinger?resource=acct:gargron@mastodon.social
///
/// You can also view the webfinger response that Mastodon sends:
/// https://radical.town/.well-known/webfinger?resource=acct:felix@radical.town
async fn get_webfinger_response(
  info: Query<Params>,
  state: web::Data<ChatSharedState>,
) -> Result<HttpResponse, actix_web::Error> {
  let res = web::block(move || {
    let conn = state.pool.lock().unwrap().get()?;
    let hostname = &state.settings.lock().unwrap().hostname;
    let WEBFINGER_COMMUNITY_REGEX: Regex = Regex::new(&format!(
        "^group:([a-z0-9_]{{3, 20}})@{}$",
        hostname
    ))
      .unwrap();

    let regex_parsed = WEBFINGER_COMMUNITY_REGEX
      .captures(&info.resource)
      .map(|c| c.get(1));
    // TODO: replace this with .flatten() once we are running rust 1.40
    let regex_parsed_flattened = match regex_parsed {
      Some(s) => s,
      None => None,
    };
    let community_name = match regex_parsed_flattened {
      Some(c) => c.as_str(),
      None => return Err(format_err!("not_found")),
    };

    // Make sure the requested community exists.
    let community = match Community::read_from_name(&conn, community_name.to_string()) {
      Ok(o) => o,
      Err(_) => return Err(format_err!("not_found")),
    };

    let community_url = community.get_url(&hostname);

    Ok(json!({
    "subject": info.resource,
    "aliases": [
      community_url,
    ],
    "links": [
    {
      "rel": "http://webfinger.net/rel/profile-page",
      "type": "text/html",
      "href": community_url
    },
    {
      "rel": "self",
      "type": "application/activity+json",
      // Yes this is correct, this link doesn't include the `.json` extension
      "href": community_url
    }
    // TODO: this also needs to return the subscribe link once that's implemented
    //{
    //  "rel": "http://ostatus.org/schema/1.0/subscribe",
    //  "template": "https://my_instance.com/authorize_interaction?uri={uri}"
    //}
    ]
    }))
  })
  .await
  .map(|json| HttpResponse::Ok().json(json))
  .map_err(|_| HttpResponse::InternalServerError())?;
  Ok(res)
}
