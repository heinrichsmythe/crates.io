use std::collections::HashMap;

use conduit::{Request, Response};
use conduit_cookie::{RequestSession};
use pg::rows::Row;
use pg::types::Slice;
use rand::{thread_rng, Rng};

use {Model, Version};
use app::RequestApp;
use db::{Connection, RequestTransaction};
use krate::{Crate, EncodableCrate};
use util::errors::NotFound;
use util::{RequestUtils, CargoResult, internal, ChainError, human};
use version::EncodableVersion;
use http;

pub use self::middleware::{Middleware, RequestUser};

pub mod middleware;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: i32,
    pub gh_login: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub avatar: Option<String>,
    pub gh_access_token: String,
    pub api_token: String,
}

#[derive(RustcDecodable, RustcEncodable)]
pub struct EncodableUser {
    pub id: i32,
    pub login: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub avatar: Option<String>,
}

impl User {
    pub fn find(conn: &Connection, id: i32) -> CargoResult<User> {
        Model::find(conn, id)
    }

    pub fn find_by_login(conn: &Connection, login: &str) -> CargoResult<User> {
        let stmt = try!(conn.prepare("SELECT * FROM users
                                      WHERE gh_login = $1"));
        let rows = try!(stmt.query(&[&login]));
        let row = try!(rows.iter().next().chain_error(|| {
            NotFound
        }));
        Ok(Model::from_row(&row))
    }

    pub fn find_by_api_token(conn: &Connection, token: &str) -> CargoResult<User> {
        let stmt = try!(conn.prepare("SELECT * FROM users \
                                      WHERE api_token = $1 LIMIT 1"));
        return try!(stmt.query(&[&token])).iter().next()
                        .map(|r| Model::from_row(&r)).chain_error(|| {
            NotFound
        })
    }

    pub fn find_or_insert(conn: &Connection,
                          login: &str,
                          email: Option<&str>,
                          name: Option<&str>,
                          avatar: Option<&str>,
                          access_token: &str,
                          api_token: &str) -> CargoResult<User> {
        // TODO: this is racy, but it looks like any other solution is...
        //       interesting! For now just do the racy thing which will report
        //       more errors than it needs to.

        let stmt = try!(conn.prepare("UPDATE users
                                      SET gh_access_token = $1,
                                          email = $2,
                                          name = $3,
                                          gh_avatar = $4
                                      WHERE gh_login = $5
                                      RETURNING *"));
        let rows = try!(stmt.query(&[&access_token,
                                     &email, &name, &avatar,
                                     &login]));
        match rows.iter().next() {
            Some(ref row) => return Ok(Model::from_row(row)),
            None => {}
        }
        let stmt = try!(conn.prepare("INSERT INTO users
                                      (email, gh_access_token, api_token,
                                       gh_login, name, gh_avatar)
                                      VALUES ($1, $2, $3, $4, $5, $6)
                                      RETURNING *"));
        let rows = try!(stmt.query(&[&email,
                                     &access_token,
                                     &api_token,
                                     &login,
                                     &name, &avatar]));
        Ok(Model::from_row(&try!(rows.iter().next().chain_error(|| {
            internal("no user with email we just found")
        }))))
    }

    pub fn new_api_token() -> String {
        thread_rng().gen_ascii_chars().take(32).collect()
    }

    pub fn encodable(self) -> EncodableUser {
        let User { id, email, api_token: _, gh_access_token: _,
                   name, gh_login, avatar } = self;
        EncodableUser {
            id: id,
            email: email,
            avatar: avatar,
            login: gh_login,
            name: name,
        }
    }
}

impl Model for User {
    fn from_row(row: &Row) -> User {
        User {
            id: row.get("id"),
            email: row.get("email"),
            gh_access_token: row.get("gh_access_token"),
            api_token: row.get("api_token"),
            gh_login: row.get("gh_login"),
            name: row.get("name"),
            avatar: row.get("gh_avatar"),
        }
    }

    fn table_name(_: Option<User>) -> &'static str { "users" }
}

pub fn github_authorize(req: &mut Request) -> CargoResult<Response> {
    let state: String = thread_rng().gen_ascii_chars().take(16).collect();
    req.session().insert("github_oauth_state".to_string(), state.clone());

    let url = req.app().github.authorize_url(state.clone());

    #[derive(RustcEncodable)]
    struct R { url: String, state: String }
    Ok(req.json(&R { url: url.to_string(), state: state }))
}

pub fn github_access_token(req: &mut Request) -> CargoResult<Response> {
    // Parse the url query
    let mut query = req.query();
    let code = query.remove("code").unwrap_or(String::new());
    let state = query.remove("state").unwrap_or(String::new());

    // Make sure that the state we just got matches the session state that we
    // should have issued earlier.
    {
        let session_state = req.session().remove(&"github_oauth_state".to_string());
        let session_state = session_state.as_ref().map(|a| &a[..]);
        if Some(&state[..]) != session_state {
            return Err(human("invalid state parameter"))
        }
    }

    #[derive(RustcDecodable)]
    struct GithubUser {
        email: Option<String>,
        name: Option<String>,
        login: String,
        avatar_url: Option<String>,
    }

    // Fetch the access token from github using the code we just got
    let token = match req.app().github.exchange(code.clone()) {
        Ok(token) => token,
        Err(s) => return Err(human(s)),
    };

    let resp = try!(http::github(req.app(), "http://api.github.com/user", &token));
    let ghuser: GithubUser = try!(http::parse_github_response(resp));

    // Into the database!
    let api_token = User::new_api_token();
    let user = try!(User::find_or_insert(try!(req.tx()),
                                         &ghuser.login,
                                         ghuser.email.as_ref()
                                               .map(|s| &s[..]),
                                         ghuser.name.as_ref()
                                               .map(|s| &s[..]),
                                         ghuser.avatar_url.as_ref()
                                               .map(|s| &s[..]),
                                         &token.access_token,
                                         &api_token));
    req.session().insert("user_id".to_string(), user.id.to_string());
    req.mut_extensions().insert(user);
    me(req)
}

pub fn logout(req: &mut Request) -> CargoResult<Response> {
    req.session().remove(&"user_id".to_string());
    Ok(req.json(&true))
}

pub fn reset_token(req: &mut Request) -> CargoResult<Response> {
    let user = try!(req.user());

    let token = User::new_api_token();
    let conn = try!(req.tx());
    try!(conn.execute("UPDATE users SET api_token = $1 WHERE id = $2",
                      &[&token, &user.id]));

    #[derive(RustcEncodable)]
    struct R { api_token: String }
    Ok(req.json(&R { api_token: token }))
}

pub fn me(req: &mut Request) -> CargoResult<Response> {
    let user = try!(req.user());

    #[derive(RustcEncodable)]
    struct R { user: EncodableUser, api_token: String }
    let token = user.api_token.clone();
    Ok(req.json(&R{ user: user.clone().encodable(), api_token: token }))
}

pub fn updates(req: &mut Request) -> CargoResult<Response> {
    let user = try!(req.user());
    let (offset, limit) = try!(req.pagination(10, 100));
    let tx = try!(req.tx());
    let sql = "SELECT versions.* FROM versions
               INNER JOIN follows
                  ON follows.user_id = $1 AND
                     follows.crate_id = versions.crate_id
               ORDER BY versions.created_at DESC OFFSET $2 LIMIT $3";

    // Load all versions
    let stmt = try!(tx.prepare(sql));
    let mut versions = Vec::new();
    let mut crate_ids = Vec::new();
    for row in try!(stmt.query(&[&user.id, &offset, &limit])) {
        let version: Version = Model::from_row(&row);
        crate_ids.push(version.crate_id);
        versions.push(version);
    }

    // Load all crates
    let mut map = HashMap::new();
    let mut crates = Vec::new();
    if crate_ids.len() > 0 {
        let stmt = try!(tx.prepare("SELECT * FROM crates WHERE id = ANY($1)"));
        for row in try!(stmt.query(&[&Slice(&crate_ids)])) {
            let krate: Crate = Model::from_row(&row);
            map.insert(krate.id, krate.name.clone());
            crates.push(krate);
        }
    }

    // Encode everything!
    let crates = crates.into_iter().map(|c| c.encodable(None)).collect();
    let versions = versions.into_iter().map(|v| {
        let id = v.crate_id;
        v.encodable(&map[&id])
    }).collect();

    // Check if we have another
    let sql = format!("SELECT 1 WHERE EXISTS({})", sql);
    let stmt = try!(tx.prepare(&sql));
    let more = try!(stmt.query(&[&user.id, &(offset + limit), &limit]))
                  .iter().next().is_some();

    #[derive(RustcEncodable)]
    struct R {
        versions: Vec<EncodableVersion>,
        crates: Vec<EncodableCrate>,
        meta: Meta,
    }
    #[derive(RustcEncodable)]
    struct Meta { more: bool }
    Ok(req.json(&R{ versions: versions, crates: crates, meta: Meta { more: more } }))
}
