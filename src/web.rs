// This file is part of Moonfire NVR, a security camera digital video recorder.
// Copyright (C) 2016 Scott Lamb <slamb@slamb.org>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// In addition, as a special exception, the copyright holders give
// permission to link the code of portions of this program with the
// OpenSSL library under certain conditions as described in each
// individual source file, and distribute linked combinations including
// the two.
//
// You must obey the GNU General Public License in all respects for all
// of the code used other than OpenSSL. If you modify file(s) with this
// exception, you may extend this exception to your version of the
// file(s), but you are not obligated to do so. If you do not wish to do
// so, delete this exception statement from your version. If you delete
// this exception statement from all source files in the program, then
// also delete it here.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use base::clock::Clocks;
use base::{ErrorKind, ResultExt, bail_t, strutil};
use bytes::Bytes;
use crate::body::{Body, BoxedError};
use crate::json;
use crate::mp4;
use base64;
use bytes::{BufMut, BytesMut};
use core::borrow::Borrow;
use core::str::FromStr;
use db::{auth, recording};
use db::dir::SampleFileDir;
use failure::{Error, bail, format_err};
use fnv::FnvHashMap;
use futures::future::{self, Future, TryFutureExt};
use futures::stream::{Stream, StreamExt, TryStreamExt};
use http::{Request, Response, status::StatusCode};
use http_serve;
use http::header::{self, HeaderValue};
use lazy_static::lazy_static;
use log::{debug, info, warn};
use regex::Regex;
use serde_json;
use std::collections::HashMap;
use std::cmp;
use std::fs;
use std::net::IpAddr;
use std::ops::Range;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use url::form_urlencoded;
use uuid::Uuid;

lazy_static! {
    /// Regex used to parse the `s` query parameter to `view.mp4`.
    /// As described in `design/api.md`, this is of the form
    /// `START_ID[-END_ID][@OPEN_ID][.[REL_START_TIME]-[REL_END_TIME]]`.
    static ref SEGMENTS_RE: Regex =
        Regex::new(r"^(\d+)(-\d+)?(@\d+)?(?:\.(\d+)?-(\d+)?)?$").unwrap();
}

type BoxedFuture = Box<dyn Future<Output = Result<Response<Body>, BoxedError>> +
                       Sync + Send + 'static>;

#[derive(Debug, Eq, PartialEq)]
enum Path {
    TopLevel,                                         // "/api/"
    Request,                                          // "/api/request"
    InitSegment([u8; 20], bool),                      // "/api/init/<sha1>.mp4{.txt}"
    Camera(Uuid),                                     // "/api/cameras/<uuid>/"
    Signals,                                          // "/api/signals"
    StreamRecordings(Uuid, db::StreamType),           // "/api/cameras/<uuid>/<type>/recordings"
    StreamViewMp4(Uuid, db::StreamType, bool),        // "/api/cameras/<uuid>/<type>/view.mp4{.txt}"
    StreamViewMp4Segment(Uuid, db::StreamType, bool), // "/api/cameras/<uuid>/<type>/view.m4s{.txt}"
    StreamLiveMp4Segments(Uuid, db::StreamType),      // "/api/cameras/<uuid>/<type>/live.m4s"
    Login,                                            // "/api/login"
    Logout,                                           // "/api/logout"
    Static,                                           // (anything that doesn't start with "/api/")
    NotFound,
}

impl Path {
    fn decode(path: &str) -> Self {
        if !path.starts_with("/api/") {
            return Path::Static;
        }
        let path = &path["/api".len()..];
        if path == "/" {
            return Path::TopLevel;
        }
        match path {
            "/login" => return Path::Login,
            "/logout" => return Path::Logout,
            "/request" => return Path::Request,
            "/signals" => return Path::Signals,
            _ => {},
        };
        if path.starts_with("/init/") {
            let (debug, path) = if path.ends_with(".txt") {
                (true, &path[0 .. path.len() - 4])
            } else {
                (false, path)
            };
            if path.len() != 50 || !path.ends_with(".mp4") {
                return Path::NotFound;
            }
            if let Ok(sha1) = strutil::dehex(&path.as_bytes()[6..46]) {
                return Path::InitSegment(sha1, debug);
            }
            return Path::NotFound;
        }
        if !path.starts_with("/cameras/") {
            return Path::NotFound;
        }
        let path = &path["/cameras/".len()..];
        let slash = match path.find('/') {
            None => { return Path::NotFound; },
            Some(s) => s,
        };
        let uuid = &path[0 .. slash];
        let path = &path[slash+1 .. ];

        // TODO(slamb): require uuid to be in canonical format.
        let uuid = match Uuid::parse_str(uuid) {
            Ok(u) => u,
            Err(_) => { return Path::NotFound },
        };

        if path.is_empty() {
            return Path::Camera(uuid);
        }

        let slash = match path.find('/') {
            None => { return Path::NotFound; },
            Some(s) => s,
        };
        let (type_, path) = path.split_at(slash);

        let type_ = match db::StreamType::parse(type_) {
            None => { return Path::NotFound; },
            Some(t) => t,
        };
        match path {
            "/recordings" => Path::StreamRecordings(uuid, type_),
            "/view.mp4" => Path::StreamViewMp4(uuid, type_, false),
            "/view.mp4.txt" => Path::StreamViewMp4(uuid, type_, true),
            "/view.m4s" => Path::StreamViewMp4Segment(uuid, type_, false),
            "/view.m4s.txt" => Path::StreamViewMp4Segment(uuid, type_, true),
            "/live.m4s" => Path::StreamLiveMp4Segments(uuid, type_),
            _ => Path::NotFound,
        }
    }
}

fn plain_response<B: Into<Body>>(status: http::StatusCode, body: B) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"))
        .body(body.into()).expect("hardcoded head should be valid")
}

fn not_found<B: Into<Body>>(body: B) -> Response<Body> {
    plain_response(StatusCode::NOT_FOUND, body)
}

fn bad_req<B: Into<Body>>(body: B) -> Response<Body> {
    plain_response(StatusCode::BAD_REQUEST, body)
}

fn internal_server_err<E: Into<Error>>(err: E) -> Response<Body> {
    plain_response(StatusCode::INTERNAL_SERVER_ERROR, err.into().to_string())
}

fn from_base_error(err: base::Error) -> Response<Body> {
    let status_code = match err.kind() {
        ErrorKind::PermissionDenied | ErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorKind::InvalidArgument => StatusCode::BAD_REQUEST,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    plain_response(status_code, err.to_string())
}

#[derive(Debug, Eq, PartialEq)]
struct Segments {
    ids: Range<i32>,
    open_id: Option<u32>,
    start_time: i64,
    end_time: Option<i64>,
}

impl Segments {
    pub fn parse(input: &str) -> Result<Segments, ()> {
        let caps = SEGMENTS_RE.captures(input).ok_or(())?;
        let ids_start = i32::from_str(caps.get(1).unwrap().as_str()).map_err(|_| ())?;
        let ids_end = match caps.get(2) {
            Some(m) => i32::from_str(&m.as_str()[1..]).map_err(|_| ())?,
            None => ids_start,
        } + 1;
        let open_id = match caps.get(3) {
            Some(m) => Some(u32::from_str(&m.as_str()[1..]).map_err(|_| ())?),
            None => None,
        };
        if ids_start < 0 || ids_end <= ids_start {
            return Err(());
        }
        let start_time = caps.get(4).map_or(Ok(0), |m| i64::from_str(m.as_str())).map_err(|_| ())?;
        if start_time < 0 {
            return Err(());
        }
        let end_time = match caps.get(5) {
            Some(v) => {
                let e = i64::from_str(v.as_str()).map_err(|_| ())?;
                if e <= start_time {
                    return Err(());
                }
                Some(e)
            },
            None => None
        };
        Ok(Segments {
            ids: ids_start .. ids_end,
            open_id,
            start_time,
            end_time,
        })
    }
}

/// A user interface file (.html, .js, etc).
/// The list of files is loaded into the server at startup; this makes path canonicalization easy.
/// The files themselves are opened on every request so they can be changed during development.
#[derive(Debug)]
struct UiFile {
    mime: HeaderValue,
    path: PathBuf,
}

struct Caller {
    permissions: db::Permissions,
    session: Option<json::Session>,
}

impl Caller {
}

struct ServiceInner {
    db: Arc<db::Database>,
    dirs_by_stream_id: Arc<FnvHashMap<i32, Arc<SampleFileDir>>>,
    ui_files: HashMap<String, UiFile>,
    time_zone_name: String,
    allow_unauthenticated_permissions: Option<db::Permissions>,
    trust_forward_hdrs: bool,
}

type ResponseResult = Result<Response<Body>, Response<Body>>;

fn serve_json<T: serde::ser::Serialize>(req: &Request<hyper::Body>, out: &T) -> ResponseResult {
    let (mut resp, writer) = http_serve::streaming_body(&req).build();
    resp.headers_mut().insert(header::CONTENT_TYPE,
                              HeaderValue::from_static("application/json"));
    if let Some(mut w) = writer {
        serde_json::to_writer(&mut w, out).map_err(internal_server_err)?;
    }
    Ok(resp)
}

impl ServiceInner {
    fn top_level(&self, req: &Request<::hyper::Body>, caller: Caller) -> ResponseResult {
        let mut days = false;
        let mut camera_configs = false;
        if let Some(q) = req.uri().query() {
            for (key, value) in form_urlencoded::parse(q.as_bytes()) {
                let (key, value): (_, &str) = (key.borrow(), value.borrow());
                match key {
                    "days" => days = value == "true",
                    "cameraConfigs" => camera_configs = value == "true",
                    _ => {},
                };
            }
        }

        if camera_configs {
            if !caller.permissions.read_camera_configs {
                return Err(plain_response(StatusCode::UNAUTHORIZED,
                                          "read_camera_configs required"));
            }
        }

        let db = self.db.lock();
        serve_json(req, &json::TopLevel {
            time_zone_name: &self.time_zone_name,
            cameras: (&db, days, camera_configs),
            session: caller.session,
            signals: (&db, days),
            signal_types: &db,
        })
    }

    fn camera(&self, req: &Request<::hyper::Body>, uuid: Uuid) -> ResponseResult {
        let db = self.db.lock();
        let camera = db.get_camera(uuid)
                       .ok_or_else(|| not_found(format!("no such camera {}", uuid)))?;
        serve_json(req, &json::Camera::wrap(camera, &db, true, false).map_err(internal_server_err)?)
    }

    fn stream_recordings(&self, req: &Request<::hyper::Body>, uuid: Uuid, type_: db::StreamType)
                         -> ResponseResult {
        let (r, split) = {
            let mut time = recording::Time::min_value() .. recording::Time::max_value();
            let mut split = recording::Duration(i64::max_value());
            if let Some(q) = req.uri().query() {
                for (key, value) in form_urlencoded::parse(q.as_bytes()) {
                    let (key, value) = (key.borrow(), value.borrow());
                    match key {
                        "startTime90k" => {
                            time.start = recording::Time::parse(value)
                                .map_err(|_| bad_req("unparseable startTime90k"))?
                        },
                        "endTime90k" => {
                            time.end = recording::Time::parse(value)
                                .map_err(|_| bad_req("unparseable endTime90k"))?
                        },
                        "split90k" => {
                            split = recording::Duration(i64::from_str(value)
                                .map_err(|_| bad_req("unparseable split90k"))?)
                        },
                        _ => {},
                    }
                };
            }
            (time, split)
        };
        let mut out = json::ListRecordings{recordings: Vec::new()};
        {
            let db = self.db.lock();
            let camera = db.get_camera(uuid)
                           .ok_or_else(|| plain_response(StatusCode::NOT_FOUND,
                                                         format!("no such camera {}", uuid)))?;
            let stream_id = camera.streams[type_.index()]
                .ok_or_else(|| plain_response(StatusCode::NOT_FOUND,
                                              format!("no such stream {}/{}", uuid, type_)))?;
            db.list_aggregated_recordings(stream_id, r, split, &mut |row| {
                let end = row.ids.end - 1;  // in api, ids are inclusive.
                let vse = db.video_sample_entries_by_id().get(&row.video_sample_entry_id).unwrap();
                out.recordings.push(json::Recording {
                    start_id: row.ids.start,
                    end_id: if end == row.ids.start { None } else { Some(end) },
                    start_time_90k: row.time.start.0,
                    end_time_90k: row.time.end.0,
                    sample_file_bytes: row.sample_file_bytes,
                    open_id: row.open_id,
                    first_uncommitted: row.first_uncommitted,
                    video_samples: row.video_samples,
                    video_sample_entry_width: vse.width,
                    video_sample_entry_height: vse.height,
                    video_sample_entry_sha1: strutil::hex(&vse.sha1),
                    growing: row.growing,
                });
                Ok(())
            }).map_err(internal_server_err)?;
        }
        serve_json(req, &out)
    }

    fn init_segment(&self, sha1: [u8; 20], debug: bool, req: &Request<::hyper::Body>)
                    -> ResponseResult {
        let mut builder = mp4::FileBuilder::new(mp4::Type::InitSegment);
        let db = self.db.lock();
        for ent in db.video_sample_entries_by_id().values() {
            if ent.sha1 == sha1 {
                builder.append_video_sample_entry(ent.clone());
                let mp4 = builder.build(self.db.clone(), self.dirs_by_stream_id.clone())
                    .map_err(from_base_error)?;
                if debug {
                    return Ok(plain_response(StatusCode::OK, format!("{:#?}", mp4)));
                } else {
                    return Ok(http_serve::serve(mp4, req));
                }
            }
        }
        Err(not_found("no such init segment"))
    }

    fn stream_view_mp4(&self, req: &Request<::hyper::Body>, caller: Caller, uuid: Uuid,
                       stream_type: db::StreamType, mp4_type: mp4::Type, debug: bool)
                       -> ResponseResult {
        if !caller.permissions.view_video {
            return Err(plain_response(StatusCode::UNAUTHORIZED, "view_video required"));
        }
        let stream_id = {
            let db = self.db.lock();
            let camera = db.get_camera(uuid)
                           .ok_or_else(|| plain_response(StatusCode::NOT_FOUND,
                                                         format!("no such camera {}", uuid)))?;
            camera.streams[stream_type.index()]
                .ok_or_else(|| plain_response(StatusCode::NOT_FOUND,
                                              format!("no such stream {}/{}", uuid,
                                                      stream_type)))?
        };
        let mut builder = mp4::FileBuilder::new(mp4_type);
        if let Some(q) = req.uri().query() {
            for (key, value) in form_urlencoded::parse(q.as_bytes()) {
                let (key, value) = (key.borrow(), value.borrow());
                match key {
                    "s" => {
                        let s = Segments::parse(value).map_err(
                            |()| plain_response(StatusCode::BAD_REQUEST,
                                                format!("invalid s parameter: {}", value)))?;
                        debug!("stream_view_mp4: appending s={:?}", s);
                        let mut est_segments = (s.ids.end - s.ids.start) as usize;
                        if let Some(end) = s.end_time {
                            // There should be roughly ceil((end - start) /
                            // desired_recording_duration) recordings in the desired timespan if
                            // there are no gaps or overlap, possibly another for misalignment of
                            // the requested timespan with the rotate offset and another because
                            // rotation only happens at key frames.
                            let ceil_durations = (end - s.start_time +
                                                  recording::DESIRED_RECORDING_DURATION - 1) /
                                                 recording::DESIRED_RECORDING_DURATION;
                            est_segments = cmp::min(est_segments, (ceil_durations + 2) as usize);
                        }
                        builder.reserve(est_segments);
                        let db = self.db.lock();
                        let mut prev = None;
                        let mut cur_off = 0;
                        db.list_recordings_by_id(stream_id, s.ids.clone(), &mut |r| {
                            let recording_id = r.id.recording();

                            if let Some(o) = s.open_id {
                                if r.open_id != o {
                                    bail!("recording {} has open id {}, requested {}",
                                          r.id, r.open_id, o);
                                }
                            }

                            // Check for missing recordings.
                            match prev {
                                None if recording_id == s.ids.start => {},
                                None => bail!("no such recording {}/{}", stream_id, s.ids.start),
                                Some(id) if r.id.recording() != id + 1 => {
                                    bail!("no such recording {}/{}", stream_id, id + 1);
                                },
                                _ => {},
                            };
                            prev = Some(recording_id);

                            // Add a segment for the relevant part of the recording, if any.
                            let end_time = s.end_time.unwrap_or(i64::max_value());
                            let d = r.duration_90k as i64;
                            if s.start_time <= cur_off + d && cur_off < end_time {
                                let start = cmp::max(0, s.start_time - cur_off);
                                let end = cmp::min(d, end_time - cur_off);
                                let times = start as i32 .. end as i32;
                                debug!("...appending recording {} with times {:?} \
                                       (out of dur {})", r.id, times, d);
                                builder.append(&db, r, start as i32 .. end as i32)?;
                            } else {
                                debug!("...skipping recording {} dur {}", r.id, d);
                            }
                            cur_off += d;
                            Ok(())
                        }).map_err(internal_server_err)?;

                        // Check for missing recordings.
                        match prev {
                            Some(id) if s.ids.end != id + 1 => {
                                return Err(not_found(format!("no such recording {}/{}",
                                                             stream_id, s.ids.end - 1)));
                            },
                            None => {
                                return Err(not_found(format!("no such recording {}/{}",
                                                             stream_id, s.ids.start)));
                            },
                            _ => {},
                        };
                        if let Some(end) = s.end_time {
                            if end > cur_off {
                                return Err(plain_response(
                                        StatusCode::BAD_REQUEST,
                                        format!("end time {} is beyond specified recordings",
                                                end)));
                            }
                        }
                    },
                    "ts" => builder.include_timestamp_subtitle_track(value == "true"),
                    _ => return Err(bad_req(format!("parameter {} not understood", key))),
                }
            };
        }
        let mp4 = builder.build(self.db.clone(), self.dirs_by_stream_id.clone())
                         .map_err(from_base_error)?;
        if debug {
            return Ok(plain_response(StatusCode::OK, format!("{:#?}", mp4)));
        }
        Ok(http_serve::serve(mp4, req))
    }

    fn static_file(&self, req: &Request<::hyper::Body>, path: &str) -> ResponseResult {
        let s = self.ui_files.get(path).ok_or_else(|| not_found("no such static file"))?;
        let f = tokio::task::block_in_place(move || {
            fs::File::open(&s.path).map_err(internal_server_err)
        })?;
        let mut hdrs = http::HeaderMap::new();
        hdrs.insert(header::CONTENT_TYPE, s.mime.clone());
        let e = http_serve::ChunkedReadFile::new(f, hdrs).map_err(internal_server_err)?;
        Ok(http_serve::serve(e, &req))
    }

    fn authreq(&self, req: &Request<::hyper::Body>) -> auth::Request {
        auth::Request {
            when_sec: Some(self.db.clocks().realtime().sec),
            addr: if self.trust_forward_hdrs {
                req.headers().get("X-Real-IP")
                   .and_then(|v| v.to_str().ok())
                   .and_then(|v| IpAddr::from_str(v).ok())
            } else { None },
            user_agent: req.headers().get(header::USER_AGENT).map(|ua| ua.as_bytes().to_vec()),
        }
    }

    fn request(&self, req: &Request<::hyper::Body>) -> ResponseResult {
        let authreq = self.authreq(req);
        let host = req.headers().get(header::HOST).map(|h| String::from_utf8_lossy(h.as_bytes()));
        let agent = authreq.user_agent.as_ref().map(|u| String::from_utf8_lossy(&u[..]));
        Ok(plain_response(StatusCode::OK, format!(
                    "when: {}\n\
                    host: {:?}\n\
                    addr: {:?}\n\
                    user_agent: {:?}\n\
                    secure: {:?}",
                    time::at(time::Timespec{sec: authreq.when_sec.unwrap(), nsec: 0})
                             .strftime("%FT%T")
                             .map(|f| f.to_string())
                             .unwrap_or_else(|e| e.to_string()),
                    host.as_ref().map(|h| &*h),
                    &authreq.addr,
                    agent.as_ref().map(|a| &*a),
                    self.is_secure(req))))
    }

    fn is_secure(&self, req: &Request<::hyper::Body>) -> bool {
        self.trust_forward_hdrs &&
            req.headers().get("X-Forwarded-Proto")
               .map(|v| v.as_bytes() == b"https")
               .unwrap_or(false)
    }

    fn login(&self, req: &Request<::hyper::Body>, body: Bytes) -> ResponseResult {
        let r: json::LoginRequest = serde_json::from_slice(&body)
            .map_err(|e| bad_req(e.to_string()))?;
        let authreq = self.authreq(req);
        let host = req.headers().get(header::HOST).ok_or_else(|| bad_req("missing Host header!"))?;
        let host = host.as_bytes();
        let domain = match ::memchr::memchr(b':', host) {
            Some(colon) => &host[0..colon],
            None => host,
        }.to_owned();
        let mut l = self.db.lock();
        let is_secure = self.is_secure(req);
        let flags = (auth::SessionFlags::HttpOnly as i32) |
                    (auth::SessionFlags::SameSite as i32) |
                    (auth::SessionFlags::SameSiteStrict as i32) |
                    if is_secure { (auth::SessionFlags::Secure as i32) } else { 0 };
        let (sid, _) = l.login_by_password(authreq, &r.username, r.password, Some(domain),
            flags)
            .map_err(|e| plain_response(StatusCode::UNAUTHORIZED, e.to_string()))?;
        let s_suffix = if is_secure {
            &b"; HttpOnly; Secure; SameSite=Strict; Max-Age=2147483648; Path=/"[..]
        } else {
            &b"; HttpOnly; SameSite=Strict; Max-Age=2147483648; Path=/"[..]
        };
        let mut encoded = [0u8; 64];
        base64::encode_config_slice(&sid, base64::STANDARD_NO_PAD, &mut encoded);
        let mut cookie = BytesMut::with_capacity("s=".len() + encoded.len() + s_suffix.len());
        cookie.put(&b"s="[..]);
        cookie.put(&encoded[..]);
        cookie.put(s_suffix);
        Ok(Response::builder()
            .header(header::SET_COOKIE, HeaderValue::from_maybe_shared(cookie.freeze())
                                        .expect("cookie can't have invalid bytes"))
            .status(StatusCode::NO_CONTENT)
            .body(b""[..].into()).unwrap())
    }

    fn logout(&self, req: &Request<hyper::Body>, body: Bytes) -> ResponseResult {
        let r: json::LogoutRequest = serde_json::from_slice(&body)
            .map_err(|e| bad_req(e.to_string()))?;

        let mut res = Response::new(b""[..].into());
        if let Some(sid) = extract_sid(req) {
            let authreq = self.authreq(req);
            let mut l = self.db.lock();
            let hash = sid.hash();
            let need_revoke = match l.authenticate_session(authreq.clone(), &hash) {
                Ok((s, _)) => {
                    if !csrf_matches(r.csrf, s.csrf()) {
                        warn!("logout request with missing/incorrect csrf");
                        return Err(bad_req("logout with incorrect csrf token"));
                    }
                    info!("revoking session");
                    true
                },
                Err(e) => {
                    // TODO: distinguish "no such session", "session is no longer valid", and
                    // "user ... is disabled" (which are all client error / bad state) from database
                    // errors.
                    warn!("logout failed: {}", e);
                    false
                },
            };
            if need_revoke {
                // TODO: inline this above with non-lexical lifetimes.
                l.revoke_session(auth::RevocationReason::LoggedOut, None, authreq, &hash)
                 .map_err(internal_server_err)?;
            }

            // By now the session is invalid (whether it was valid to start with or not).
            // Clear useless cookie.
            res.headers_mut().append(header::SET_COOKIE,
                                     HeaderValue::from_str("s=; Max-Age=0; Path=/").unwrap());
        }
        *res.status_mut() = StatusCode::NO_CONTENT;
        Ok(res)
    }

    fn post_signals(&self, req: &Request<hyper::Body>, caller: Caller, body: Bytes)
                    -> ResponseResult {
        if !caller.permissions.update_signals {
            return Err(plain_response(StatusCode::UNAUTHORIZED, "update_signals required"));
        }
        let r: json::PostSignalsRequest = serde_json::from_slice(&body)
            .map_err(|e| bad_req(e.to_string()))?;
        let mut l = self.db.lock();
        let now = recording::Time::new(self.db.clocks().realtime());
        let start = r.start_time_90k.map(recording::Time).unwrap_or(now);
        let end = match r.end_base {
            json::PostSignalsEndBase::Epoch => recording::Time(r.rel_end_time_90k.ok_or_else(
                || bad_req("must specify rel_end_time_90k when end_base is epoch"))?),
            json::PostSignalsEndBase::Now => {
                now + recording::Duration(r.rel_end_time_90k.unwrap_or(0))
            },
        };
        l.update_signals(start .. end, &r.signal_ids, &r.states).map_err(from_base_error)?;
        serve_json(req, &json::PostSignalsResponse {
            time_90k: now.0,
        })
    }

    fn get_signals(&self, req: &Request<hyper::Body>) -> ResponseResult {
        let mut time = recording::Time::min_value() .. recording::Time::max_value();
        if let Some(q) = req.uri().query() {
            for (key, value) in form_urlencoded::parse(q.as_bytes()) {
                let (key, value) = (key.borrow(), value.borrow());
                match key {
                    "startTime90k" => {
                        time.start = recording::Time::parse(value)
                            .map_err(|_| bad_req("unparseable startTime90k"))?
                    },
                    "endTime90k" => {
                        time.end = recording::Time::parse(value)
                            .map_err(|_| bad_req("unparseable endTime90k"))?
                    },
                    _ => {},
                }
            }
        }

        let mut signals = json::Signals::default();
        self.db.lock().list_changes_by_time(time, &mut |c: &db::signal::ListStateChangesRow| {
            signals.times_90k.push(c.when.0);
            signals.signal_ids.push(c.signal);
            signals.states.push(c.state);
        });
        serve_json(req, &signals)
    }

    fn authenticate(&self, req: &Request<hyper::Body>, unauth_path: bool)
                    -> Result<Caller, base::Error> {
        if let Some(sid) = extract_sid(req) {
            let authreq = self.authreq(req);

            // TODO: real error handling! this assumes all errors are due to lack of
            // authentication, when they could be logic errors in SQL or such.
            if let Ok((s, u)) = self.db.lock().authenticate_session(authreq.clone(), &sid.hash()) {
                return Ok(Caller {
                    permissions: s.permissions.clone(),
                    session: Some(json::Session {
                        username: u.username.clone(),
                        csrf: s.csrf(),
                    }),
                });
            }
            info!("authenticate_session failed");
        }

        if let Some(s) = self.allow_unauthenticated_permissions.as_ref() {
            return Ok(Caller {
                permissions: s.clone(),
                session: None,
            });
        }

        if unauth_path {
            return Ok(Caller {
                permissions: db::Permissions::default(),
                session: None,
            })
        }

        bail_t!(Unauthenticated, "unauthenticated");
    }
}

fn csrf_matches(csrf: &str, session: auth::SessionHash) -> bool {
    let mut b64 = [0u8; 32];
    session.encode_base64(&mut b64);
    ::ring::constant_time::verify_slices_are_equal(&b64[..], csrf.as_bytes()).is_ok()
}

/// Extracts `s` cookie from the HTTP request. Does not authenticate.
fn extract_sid(req: &Request<hyper::Body>) -> Option<auth::RawSessionId> {
    let hdr = match req.headers().get(header::COOKIE) {
        None => return None,
        Some(c) => c,
    };
    for mut cookie in hdr.as_bytes().split(|&b| b == b';') {
        if cookie.starts_with(b" ") {
            cookie = &cookie[1..];
        }
        if cookie.starts_with(b"s=") {
            let s = &cookie[2..];
            if let Ok(s) = auth::RawSessionId::decode_base64(s) {
                return Some(s);
            }
        }
    }
    None
}

/// Returns a future separating the request from its JSON body.
///
/// If this is not a `POST` or the body's `Content-Type` is not
/// `application/json`, returns an appropriate error response instead.
///
/// Use with `and_then` to chain logic which consumes the form body.
async fn with_json_body(mut req: Request<hyper::Body>)
    -> Result<(Request<hyper::Body>, Bytes), Response<Body>> {
    if *req.method() != http::method::Method::POST {
        return Err(plain_response(StatusCode::METHOD_NOT_ALLOWED, "POST expected"));
    }
    let correct_mime_type = match req.headers().get(header::CONTENT_TYPE) {
        Some(t) if t == "application/json" => true,
        Some(t) if t == "application/json; charset=UTF-8" => true,
        _ => false,
    };
    if !correct_mime_type {
        return Err(bad_req("expected application/json request body"));
    }
    let b = ::std::mem::replace(req.body_mut(), hyper::Body::empty());
    match hyper::body::to_bytes(b).await {
        Ok(b) => Ok((req, b)),
        Err(e) => Err(internal_server_err(format_err!("unable to read request body: {}", e))),
    }
}


pub struct Config<'a> {
    pub db: Arc<db::Database>,
    pub ui_dir: Option<&'a str>,
    pub trust_forward_hdrs: bool,
    pub time_zone_name: String,
    pub allow_unauthenticated_permissions: Option<db::Permissions>,
}

#[derive(Clone)]
pub struct Service(Arc<ServiceInner>);

impl Service {
    pub fn new(config: Config) -> Result<Self, Error> {
        let mut ui_files = HashMap::new();
        if let Some(d) = config.ui_dir {
            Service::fill_ui_files(d, &mut ui_files);
        }
        debug!("UI files: {:#?}", ui_files);
        let dirs_by_stream_id = {
            let l = config.db.lock();
            let mut d =
                FnvHashMap::with_capacity_and_hasher(l.streams_by_id().len(), Default::default());
            for (&id, s) in l.streams_by_id().iter() {
                let dir_id = match s.sample_file_dir_id {
                    Some(d) => d,
                    None => continue,
                };
                d.insert(id, l.sample_file_dirs_by_id()
                              .get(&dir_id)
                              .unwrap()
                              .get()?);
            }
            Arc::new(d)
        };

        Ok(Service(Arc::new(ServiceInner {
            db: config.db,
            dirs_by_stream_id,
            ui_files,
            allow_unauthenticated_permissions: config.allow_unauthenticated_permissions,
            trust_forward_hdrs: config.trust_forward_hdrs,
            time_zone_name: config.time_zone_name,
        })))
    }

    fn fill_ui_files(dir: &str, files: &mut HashMap<String, UiFile>) {
        let r = match fs::read_dir(dir) {
            Ok(r) => r,
            Err(e) => {
                warn!("Unable to search --ui-dir={}; will serve no static files. Error was: {}",
                      dir, e);
                return;
            }
        };
        for e in r {
            let e = match e {
                Ok(e) => e,
                Err(e) => {
                    warn!("Error searching UI directory; may be missing files. Error was: {}", e);
                    continue;
                },
            };
            let (p, mime) = match e.file_name().to_str() {
                Some(n) if n == "index.html" => ("/".to_owned(), "text/html"),
                Some(n) if n.ends_with(".html") => (format!("/{}", n), "text/html"),
                Some(n) if n.ends_with(".ico") => (format!("/{}", n), "image/vnd.microsoft.icon"),
                Some(n) if n.ends_with(".js") => (format!("/{}", n), "text/javascript"),
                Some(n) if n.ends_with(".map") => (format!("/{}", n), "text/javascript"),
                Some(n) if n.ends_with(".png") => (format!("/{}", n), "image/png"),
                Some(n) => {
                    warn!("UI directory file {:?} has unknown extension; skipping", n);
                    continue;
                },
                None => {
                    warn!("UI directory file {:?} is not a valid UTF-8 string; skipping",
                          e.file_name());
                    continue;
                },
            };
            files.insert(p, UiFile {
                mime: HeaderValue::from_static(mime),
                path: e.path(),
            });
        }
    }

    fn stream_live_m4s(&self, _req: &Request<::hyper::Body>, caller: Caller, uuid: Uuid,
                       stream_type: db::StreamType) -> ResponseResult {
        if !caller.permissions.view_video {
            return Err(plain_response(StatusCode::UNAUTHORIZED, "view_video required"));
        }
        let stream_id;
        let open_id;
        let (sub_tx, sub_rx) = futures::channel::mpsc::unbounded();
        {
            let mut db = self.0.db.lock();
            open_id = match db.open {
                None => return Err(plain_response(
                        StatusCode::PRECONDITION_FAILED,
                        "database is read-only; there are no live streams")),
                Some(o) => o.id,
            };
            let camera = db.get_camera(uuid)
                           .ok_or_else(|| plain_response(StatusCode::NOT_FOUND,
                                                         format!("no such camera {}", uuid)))?;
            stream_id = camera.streams[stream_type.index()]
                .ok_or_else(|| plain_response(StatusCode::NOT_FOUND,
                                              format!("no such stream {}/{}", uuid,
                                                      stream_type)))?;
            db.watch_live(stream_id, Box::new(move |l| sub_tx.unbounded_send(l).is_ok()))
                .expect("stream_id refed by camera");
        }
        let inner = self.0.clone();
        let body = sub_rx
            .map(move |live| -> Result<_, base::Error> {
                let mut builder = mp4::FileBuilder::new(mp4::Type::MediaSegment);
                let mut vse_id = None;
                {
                    let db = inner.db.lock();
                    let mut rows = 0;
                    db.list_recordings_by_id(stream_id, live.recording .. live.recording+1,
                                             &mut |r| {
                        rows += 1;
                        let vse = db.video_sample_entries_by_id().get(&r.video_sample_entry_id)
                                    .unwrap();
                        vse_id = Some(strutil::hex(&vse.sha1));
                        builder.append(&db, r, live.off_90k.clone())?;
                        Ok(())
                    }).err_kind(base::ErrorKind::Unknown)?;
                    if rows != 1 {
                        bail_t!(Internal, "unable to find {:?}", live);
                    }
                }
                let vse_id = vse_id.unwrap();
                use http_serve::Entity;
                let mp4 = builder.build(inner.db.clone(), inner.dirs_by_stream_id.clone())?;
                let mut hdrs = http::header::HeaderMap::new();
                mp4.add_headers(&mut hdrs);
                let mime_type = hdrs.get(http::header::CONTENT_TYPE).unwrap();
                let len = mp4.len();
                use futures::stream::once;
                let hdr = format!(
                    "--B\r\n\
                    Content-Length: {}\r\n\
                    Content-Type: {}\r\n\
                    X-Recording-Id: {}\r\n\
                    X-Time-Range: {}-{}\r\n\
                    X-Video-Sample-Entry-Sha1: {}\r\n\r\n",
                    len,
                    mime_type.to_str().unwrap(),
                    live.recording,
                    live.off_90k.start,
                    live.off_90k.end,
                    &vse_id);
                let v: Vec<Pin<crate::body::BodyStream>> = vec![
                    Box::pin(once(futures::future::ok(hdr.into()))),
                    Pin::from(mp4.get_range(0 .. len)),
                    Box::pin(once(futures::future::ok("\r\n\r\n".into())))
                ];
                Ok(futures::stream::iter(v).flatten())
            });
        let body = body.map_err::<BoxedError, _>(|e| Box::new(e.compat()));
        let _: &dyn Stream<Item = Result<_, BoxedError>> = &body;
        let body = body.try_flatten();
        let body: crate::body::BodyStream = Box::new(body);
        let body: Body = body.into();
        Ok(http::Response::builder()
            .header("X-Open-Id", open_id.to_string())
            .header("Content-Type", "multipart/mixed; boundary=B")
            .body(body)
            .unwrap())
    }

    fn signals(&self, req: Request<hyper::Body>, caller: Caller)
               -> Box<dyn Future<Output = Result<Response<Body>, Response<Body>>> + Send + Sync + 'static> {
        use http::method::Method;
        match *req.method() {
            Method::POST => Box::new(with_json_body(req)
                                     .and_then({
                                         let s = self.0.clone();
                                         move |(req, b)| future::ready(s.post_signals(&req, caller, b))
                                     })),
            Method::GET | Method::HEAD => Box::new(future::ready(self.0.get_signals(&req))),
            _ => Box::new(future::err(plain_response(StatusCode::METHOD_NOT_ALLOWED,
                                                     "POST, GET, or HEAD expected"))),
        }
    }

    pub fn serve(&mut self, req: Request<::hyper::Body>) -> BoxedFuture {
        fn wrap<R>(is_private: bool, r: R) -> BoxedFuture
        where R: Future<Output = Result<Response<Body>, Response<Body>>> + Send + Sync + 'static {
            return Box::new(r.or_else(|e| futures::future::ok(e)).map_ok(move |mut r| {
                if is_private {
                    r.headers_mut().insert("Cache-Control", HeaderValue::from_static("private"));
                }
                r
            }))
        }

        fn wrap_r(is_private: bool, r: ResponseResult)
               -> Box<dyn Future<Output = Result<Response<Body>, BoxedError>> + Send + Sync + 'static> {
            return wrap(is_private, future::ready(r))
        }

        let p = Path::decode(req.uri().path());
        let always_allow_unauthenticated = match p {
            Path::NotFound | Path::Request | Path::Login | Path::Logout | Path::Static => true,
            _ => false,
        };
        debug!("request on: {}: {:?}", req.uri(), p);
        let caller = match self.0.authenticate(&req, always_allow_unauthenticated) {
            Ok(c) => c,
            Err(e) => return Box::new(future::ok(from_base_error(e))),
        };
        match p {
            Path::InitSegment(sha1, debug) => wrap_r(true, self.0.init_segment(sha1, debug, &req)),
            Path::TopLevel => wrap_r(true, self.0.top_level(&req, caller)),
            Path::Request => wrap_r(true, self.0.request(&req)),
            Path::Camera(uuid) => wrap_r(true, self.0.camera(&req, uuid)),
            Path::StreamRecordings(uuid, type_) => {
                wrap_r(true, self.0.stream_recordings(&req, uuid, type_))
            },
            Path::StreamViewMp4(uuid, type_, debug) => {
                wrap_r(true, self.0.stream_view_mp4(&req, caller, uuid, type_, mp4::Type::Normal,
                                                    debug))
            },
            Path::StreamViewMp4Segment(uuid, type_, debug) => {
                wrap_r(true, self.0.stream_view_mp4(&req, caller, uuid, type_,
                                                    mp4::Type::MediaSegment, debug))
            },
            Path::StreamLiveMp4Segments(uuid, type_) => {
                wrap_r(true, self.stream_live_m4s(&req, caller, uuid, type_))
            },
            Path::NotFound => wrap(true, future::err(not_found("path not understood"))),
            Path::Login => wrap(true, with_json_body(req).and_then({
                let s = self.clone();
                move |(req, b)| future::ready(s.0.login(&req, b))
            })),
            Path::Logout => wrap(true, with_json_body(req).and_then({
                let s = self.clone();
                move |(req, b)| future::ready(s.0.logout(&req, b))
            })),
            Path::Signals => wrap(true, Pin::from(self.signals(req, caller))),
            Path::Static => wrap_r(false, self.0.static_file(&req, req.uri().path())),
        }
    }
}

#[cfg(test)]
mod tests {
    use db::testutil::{self, TestDb};
    use futures::future::FutureExt;
    use log::info;
    use std::collections::HashMap;
    use super::Segments;

    struct Server {
        db: TestDb<base::clock::RealClocks>,
        base_url: String,
        //test_camera_uuid: Uuid,
        handle: Option<::std::thread::JoinHandle<()>>,
        shutdown_tx: Option<futures::channel::oneshot::Sender<()>>,
    }

    impl Server {
        fn new(allow_unauthenticated_permissions: Option<db::Permissions>) -> Server {
            let db = TestDb::new(base::clock::RealClocks {});
            let (shutdown_tx, shutdown_rx) = futures::channel::oneshot::channel::<()>();
            let service = super::Service::new(super::Config {
                db: db.db.clone(),
                ui_dir: None,
                allow_unauthenticated_permissions,
                trust_forward_hdrs: true,
                time_zone_name: "".to_owned(),
            }).unwrap();
            let make_svc = hyper::service::make_service_fn(move |_conn| {
                futures::future::ok::<_, std::convert::Infallible>(hyper::service::service_fn({
                    let mut s = service.clone();
                    move |req| std::pin::Pin::from(s.serve(req))
                }))
            });
            let (tx, rx) = std::sync::mpsc::channel();
            let handle = ::std::thread::spawn(move || {
                let addr = ([127, 0, 0, 1], 0).into();
                let mut rt = tokio::runtime::Runtime::new().unwrap();
                let srv = rt.enter(|| {
                    hyper::server::Server::bind(&addr)
                    .tcp_nodelay(true)
                    .serve(make_svc)
                });
                let addr = srv.local_addr();  // resolve port 0 to a real ephemeral port number.
                tx.send(addr).unwrap();
                rt.block_on(srv.with_graceful_shutdown(shutdown_rx.map(|_| ()))).unwrap();
            });
            let addr = rx.recv().unwrap();

            // Create a user.
            let mut c = db::UserChange::add_user("slamb".to_owned());
            c.set_password("hunter2".to_owned());
            db.db.lock().apply_user_change(c).unwrap();

            Server {
                db,
                base_url: format!("http://{}:{}", addr.ip(), addr.port()),
                handle: Some(handle),
                shutdown_tx: Some(shutdown_tx),
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.shutdown_tx.take().unwrap().send(()).unwrap();
            self.handle.take().unwrap().join().unwrap()
        }
    }

    #[derive(Clone, Debug, Default)]
    struct SessionCookie(Option<String>);

    impl SessionCookie {
        pub fn new(headers: &reqwest::header::HeaderMap) -> Self {
            let mut c = SessionCookie::default();
            c.update(headers);
            c
        }

        pub fn update(&mut self, headers: &reqwest::header::HeaderMap) {
            for set_cookie in headers.get_all(reqwest::header::SET_COOKIE) {
                let mut set_cookie = set_cookie.to_str().unwrap().split("; ");
                let c = set_cookie.next().unwrap();
                let mut clear = false;
                for attr in set_cookie {
                    if attr == "Max-Age=0" {
                        clear = true;
                    }
                }
                if !c.starts_with("s=") {
                    panic!("unrecognized cookie");
                }
                self.0 = if clear { None } else { Some(c.to_owned()) };
            }
        }

        /// Produces a `Cookie` header value.
        pub fn header(&self) -> String {
            self.0.as_ref().map(|s| s.as_str()).unwrap_or("").to_owned()
        }
    }

    #[test]
    fn paths() {
        use super::Path;
        use uuid::Uuid;
        let cam_uuid = Uuid::parse_str("35144640-ff1e-4619-b0d5-4c74c185741c").unwrap();
        assert_eq!(Path::decode("/foo"), Path::Static);
        assert_eq!(Path::decode("/api/"), Path::TopLevel);
        assert_eq!(Path::decode("/api/init/07cec464126825088ea86a07eddd6a00afa71559.mp4"),
                   Path::InitSegment([0x07, 0xce, 0xc4, 0x64, 0x12, 0x68, 0x25, 0x08, 0x8e, 0xa8,
                                      0x6a, 0x07, 0xed, 0xdd, 0x6a, 0x00, 0xaf, 0xa7, 0x15, 0x59],
                                     false));
        assert_eq!(Path::decode("/api/init/07cec464126825088ea86a07eddd6a00afa71559.mp4.txt"),
                   Path::InitSegment([0x07, 0xce, 0xc4, 0x64, 0x12, 0x68, 0x25, 0x08, 0x8e, 0xa8,
                                      0x6a, 0x07, 0xed, 0xdd, 0x6a, 0x00, 0xaf, 0xa7, 0x15, 0x59],
                                     true));
        assert_eq!(Path::decode("/api/init/000000000000000000000000000000000000000x.mp4"),
                   Path::NotFound);  // non-hexadigit
        assert_eq!(Path::decode("/api/init/000000000000000000000000000000000000000.mp4"),
                   Path::NotFound);  // too short
        assert_eq!(Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/"),
                   Path::Camera(cam_uuid));
        assert_eq!(Path::decode("/api/cameras/asdf/"), Path::NotFound);
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/main/recordings"),
            Path::StreamRecordings(cam_uuid, db::StreamType::MAIN));
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/sub/recordings"),
            Path::StreamRecordings(cam_uuid, db::StreamType::SUB));
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/junk/recordings"),
            Path::NotFound);
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/main/view.mp4"),
            Path::StreamViewMp4(cam_uuid, db::StreamType::MAIN, false));
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/main/view.mp4.txt"),
            Path::StreamViewMp4(cam_uuid, db::StreamType::MAIN, true));
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/main/view.m4s"),
            Path::StreamViewMp4Segment(cam_uuid, db::StreamType::MAIN, false));
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/main/view.m4s.txt"),
            Path::StreamViewMp4Segment(cam_uuid, db::StreamType::MAIN, true));
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/main/live.m4s"),
            Path::StreamLiveMp4Segments(cam_uuid, db::StreamType::MAIN));
        assert_eq!(
            Path::decode("/api/cameras/35144640-ff1e-4619-b0d5-4c74c185741c/main/junk"),
            Path::NotFound);
        assert_eq!(Path::decode("/api/login"), Path::Login);
        assert_eq!(Path::decode("/api/logout"), Path::Logout);
        assert_eq!(Path::decode("/api/signals"), Path::Signals);
        assert_eq!(Path::decode("/api/junk"), Path::NotFound);
    }

    #[test]
    fn test_segments() {
        testutil::init();
        assert_eq!(Segments{ids: 1..2, open_id: None, start_time: 0, end_time: None},
                   Segments::parse("1").unwrap());
        assert_eq!(Segments{ids: 1..2, open_id: Some(42), start_time: 0, end_time: None},
                   Segments::parse("1@42").unwrap());
        assert_eq!(Segments{ids: 1..2, open_id: None, start_time: 26, end_time: None},
                   Segments::parse("1.26-").unwrap());
        assert_eq!(Segments{ids: 1..2, open_id: Some(42), start_time: 26, end_time: None},
                   Segments::parse("1@42.26-").unwrap());
        assert_eq!(Segments{ids: 1..2, open_id: None, start_time: 0, end_time: Some(42)},
                   Segments::parse("1.-42").unwrap());
        assert_eq!(Segments{ids: 1..2, open_id: None, start_time: 26, end_time: Some(42)},
                   Segments::parse("1.26-42").unwrap());
        assert_eq!(Segments{ids: 1..6, open_id: None, start_time: 0, end_time: None},
                   Segments::parse("1-5").unwrap());
        assert_eq!(Segments{ids: 1..6, open_id: None, start_time: 26, end_time: None},
                   Segments::parse("1-5.26-").unwrap());
        assert_eq!(Segments{ids: 1..6, open_id: None, start_time: 0, end_time: Some(42)},
                   Segments::parse("1-5.-42").unwrap());
        assert_eq!(Segments{ids: 1..6, open_id: None, start_time: 26, end_time: Some(42)},
                   Segments::parse("1-5.26-42").unwrap());
    }

    #[tokio::test]
    async fn unauthorized_without_cookie() {
        testutil::init();
        let s = Server::new(None);
        let cli = reqwest::Client::new();
        let resp = cli.get(&format!("{}/api/", &s.base_url)).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn login() {
        testutil::init();
        let s = Server::new(None);
        let cli = reqwest::Client::new();
        let login_url = format!("{}/api/login", &s.base_url);

        let resp = cli.get(&login_url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);

        let resp = cli.post(&login_url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

        let mut p = HashMap::new();
        p.insert("username", "slamb");
        p.insert("password", "asdf");
        let resp = cli.post(&login_url).json(&p).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

        p.insert("password", "hunter2");
        let resp = cli.post(&login_url).json(&p).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
        let cookie = SessionCookie::new(resp.headers());
        info!("cookie: {:?}", cookie);
        info!("header: {}", cookie.header());

        let resp = cli.get(&format!("{}/api/", &s.base_url))
                      .header(reqwest::header::COOKIE, cookie.header())
                      .send()
                      .await
                      .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }

    #[tokio::test]
    async fn logout() {
        testutil::init();
        let s = Server::new(None);
        let cli = reqwest::Client::new();
        let mut p = HashMap::new();
        p.insert("username", "slamb");
        p.insert("password", "hunter2");
        let resp = cli.post(&format!("{}/api/login", &s.base_url)).json(&p).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
        let cookie = SessionCookie::new(resp.headers());

        // A GET shouldn't work.
        let resp = cli.get(&format!("{}/api/logout", &s.base_url))
                      .header(reqwest::header::COOKIE, cookie.header())
                      .send()
                      .await
                      .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);

        // Neither should a POST without a csrf token.
        let resp = cli.post(&format!("{}/api/logout", &s.base_url))
                      .header(reqwest::header::COOKIE, cookie.header())
                      .send()
                      .await
                      .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);

        // But it should work with the csrf token.
        // Retrieve that from the toplevel API request.
        let toplevel: serde_json::Value = cli.post(&format!("{}/api/", &s.base_url))
                                             .header(reqwest::header::COOKIE, cookie.header())
                                             .send().await.unwrap()
                                             .json().await.unwrap();
        let csrf = toplevel.get("session").unwrap().get("csrf").unwrap().as_str();
        let mut p = HashMap::new();
        p.insert("csrf", csrf);
        let resp = cli.post(&format!("{}/api/logout", &s.base_url))
                      .header(reqwest::header::COOKIE, cookie.header())
                      .json(&p)
                      .send()
                      .await
                      .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
        let mut updated_cookie = cookie.clone();
        updated_cookie.update(resp.headers());

        // The cookie should be cleared client-side.
        assert!(updated_cookie.0.is_none());

        // It should also be invalidated server-side.
        let resp = cli.get(&format!("{}/api/", &s.base_url))
                      .header(reqwest::header::COOKIE, cookie.header())
                      .send()
                      .await
                      .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn view_without_segments() {
        testutil::init();
        let mut permissions = db::Permissions::new();
        permissions.view_video = true;
        let s = Server::new(Some(permissions));
        let cli = reqwest::Client::new();
        let resp = cli.get(
            &format!("{}/api/cameras/{}/main/view.mp4", &s.base_url, s.db.test_camera_uuid))
            .send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    }
}

#[cfg(all(test, feature="nightly"))]
mod bench {
    extern crate test;

    use db::testutil::{self, TestDb};
    use hyper;
    use lazy_static::lazy_static;
    use uuid::Uuid;

    struct Server {
        base_url: String,
        test_camera_uuid: Uuid,
    }

    impl Server {
        fn new() -> Server {
            let db = TestDb::new(::base::clock::RealClocks {});
            let test_camera_uuid = db.test_camera_uuid;
            testutil::add_dummy_recordings_to_db(&db.db, 1440);
            let service = super::Service::new(super::Config {
                db: db.db.clone(),
                ui_dir: None,
                allow_unauthenticated_permissions: Some(db::Permissions::default()),
                trust_forward_hdrs: false,
                time_zone_name: "".to_owned(),
            }).unwrap();
            let make_svc = hyper::service::make_service_fn(move |_conn| {
                futures::future::ok::<_, std::convert::Infallible>(hyper::service::service_fn({
                    let mut s = service.clone();
                    move |req| std::pin::Pin::from(s.serve(req))
                }))
            });
            let mut rt = tokio::runtime::Runtime::new().unwrap();
            let srv = rt.enter(|| {
                let addr = ([127, 0, 0, 1], 0).into();
                hyper::server::Server::bind(&addr)
                    .tcp_nodelay(true)
                    .serve(make_svc)
            });
            let addr = srv.local_addr();  // resolve port 0 to a real ephemeral port number.
            ::std::thread::spawn(move || {
                rt.block_on(srv).unwrap();
            });
            Server {
                base_url: format!("http://{}:{}", addr.ip(), addr.port()),
                test_camera_uuid,
            }
        }
    }

    lazy_static! {
        static ref SERVER: Server = { Server::new() };
    }

    #[bench]
    fn serve_stream_recordings(b: &mut test::Bencher) {
        testutil::init();
        let server = &*SERVER;
        let url = reqwest::Url::parse(&format!("{}/api/cameras/{}/main/recordings", server.base_url,
                                               server.test_camera_uuid)).unwrap();
        let mut buf = Vec::new();
        let client = reqwest::Client::new();
        let mut f = || {
            let mut resp = client.get(url.clone()).send().unwrap();
            assert_eq!(resp.status(), reqwest::StatusCode::OK);
            buf.clear();
            use std::io::Read;
            resp.read_to_end(&mut buf).unwrap();
        };
        f();  // warm.
        b.iter(f);
    }
}
