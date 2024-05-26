use std::env;

use events::Broadcasters;
use sqlx::{Pool, Sqlite};
use tracing_subscriber::fmt::format::FmtSpan;
use warp::Filter;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| "tracing=info,warp=debug,ordo=debug".to_owned());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(FmtSpan::CLOSE)
        .init();

    let db = env::var("DATABASE_URL")?;

    let conn: Pool<Sqlite> = Pool::connect(&db).await?;

    sqlx::migrate!().run(&conn).await?;

    let broadcasters = Broadcasters::new();

    let routes = routes(conn, broadcasters);
    let static_files = warp::path("static").and(statics::routes());

    let routes = static_files
        .or(routes)
        .recover(rejections::handle_rejection);

    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;

    Ok(())
}

fn with_state<T: Clone + Send>(
    db: T,
) -> impl Filter<Extract = (T,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || db.clone())
}

mod statics {
    use std::path::Path;

    use include_dir::{include_dir, Dir};
    use warp::Filter;

    static STATIC_DIR: Dir = include_dir!("static");

    async fn send_file(path: warp::path::Tail) -> Result<impl warp::Reply, warp::Rejection> {
        let path = Path::new(path.as_str());
        let file = STATIC_DIR
            .get_file(path)
            .ok_or_else(warp::reject::not_found)?;

        let content_type = match file.path().extension() {
            Some(ext) if ext == "css" => "text/css",
            Some(ext) if ext == "svg" => "image/svg+xml",
            Some(ext) if ext == "js" => "text/javascript",
            _ => "application/octet-stream",
        };

        Ok(warp::reply::with_header(
            file.contents(),
            "content-type",
            content_type,
        ))
    }

    pub fn routes() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path::tail().and_then(send_file)
    }
}

pub fn routes(
    conn: sqlx::Pool<sqlx::Sqlite>,
    broadcasters: Broadcasters,
) -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    homepage::route(conn.clone())
        .or(rooms::route(conn.clone(), broadcasters.clone()))
        .or(voters::route(conn.clone(), broadcasters.clone()))
        .or(events::route(conn, broadcasters))
}

mod homepage {
    use crate::{names, rejections, utils, views, with_state};

    use maud::{html, Markup};
    use warp::Filter;

    struct Homepage {
        room_count: i32,
        voter_count: i32,
    }

    pub fn route(
        conn: sqlx::Pool<sqlx::Sqlite>,
    ) -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path::end()
            .and(warp::get())
            .and(with_state(conn))
            .and_then(handler)
    }

    async fn handler(conn: sqlx::Pool<sqlx::Sqlite>) -> Result<impl warp::Reply, warp::Rejection> {
        let rooms = sqlx::query!(r#"SELECT count(id) as count FROM rooms"#)
            .fetch_one(&conn)
            .await
            .map_err(|e| {
                tracing::error!("error while counting rooms: {e}");
                warp::reject::custom(rejections::InternalServerError)
            })?;

        let voters = sqlx::query!(r#"SELECT count(id) as count FROM voters"#)
            .fetch_one(&conn)
            .await
            .map_err(|e| {
                tracing::error!("error while counting voters: {e}");
                warp::reject::custom(rejections::InternalServerError)
            })?;

        Ok(view(Homepage {
            room_count: rooms.count,
            voter_count: voters.count,
        }))
    }

    fn view(data: Homepage) -> Markup {
        views::page(
            "Home",
            html! {
                section."two-cols h-full" {
                    div."center" {
                        div."w-500 grid gap-lg" {
                            (create_room_form())
                            (general_stats(&data))
                        }
                    }
                    div."center hide-on-small" {
                        img."w-500" src="/static/img/vote.svg";
                    }
                }
            },
        )
    }

    fn create_room_form() -> Markup {
        html! {
            form."w-full grid gap-md" hx-post=(names::rooms_url()) hx-ext="json-enc" hx-target="main" hx-swap="innerHTML" {
                div."grid gap-sm" {
                    label."text-md" { "NAME" }
                    input."input-text" name="name" required="true" min="2" placeholder="my super cool vote" {}
                }

                div."grid gap-sm" {
                    label."text-md" { "OPTIONS" }

                    div."grid gap-sm" id="options" {
                        @for _ in 0..2 {
                            input."input-text w-full" name="options" required="true" placeholder="a choice" {}
                        }
                    }

                    button."button w-fit" id="addOption" type="button" { "ADD OPTION" }
                }

                button."button w-full" type="submit" { "CREATE ROOM" }
            }
        }
    }

    fn general_stats(data: &Homepage) -> Markup {
        let room_count = utils::format_num(data.room_count);
        let room_label = utils::pluralize(data.room_count, "room", "rooms");

        let voter_count = utils::format_num(data.voter_count);
        let voter_label = utils::pluralize(data.voter_count, "voter", "voters");

        html! {
            div {
                p."text-center text-sm" { span."bold" { (room_count)  } " " (room_label)  " created so far" }
                p."text-center text-sm" { span."bold" { (voter_count) } " " (voter_label) " created so far" }
            }
        }
    }
}

mod rooms {
    use std::{collections::HashMap, time::Duration};

    use crate::{
        events::{Broadcasters, RoomEvents},
        names,
        rejections::{self, EmptyName, EmptyOption, InternalServerError, NoOptions, NotRoomAdmin},
        utils, views,
        voters::{self, VoterPage},
        voting::{self, ResultPage, Score, VoteAdminPage},
        with_state,
    };

    use maud::{html, Markup};
    use serde::Deserialize;
    use warp::{
        http::{header::SET_COOKIE, Response},
        Filter,
    };

    #[derive(Deserialize)]
    struct CreateRoomBody {
        name: String,
        options: Vec<String>,
    }

    pub fn route(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
    ) -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let create_room = with_state(conn.clone())
            .and(with_state(broadcasters.clone()))
            .and(warp::path!("rooms"))
            .and(warp::post())
            .and(warp::body::json::<CreateRoomBody>())
            .and_then(create_room)
            .with(warp::trace::named("create_room"));

        let get_room = with_state(conn.clone())
            .and(warp::path!("rooms" / i64))
            .and(warp::get())
            .and(warp::cookie::cookie(names::ROOM_ADMIN_COOKIE_NAME))
            .and_then(get_room)
            .with(warp::trace::named("get_room"));

        let join_room_page = with_state(conn.clone())
            .and(warp::path!("rooms" / i64 / "join"))
            .and(warp::get())
            .and_then(join_room_page)
            .with(warp::trace::named("join_room_page"));

        let join_room = with_state(conn.clone())
            .and(with_state(broadcasters.clone()))
            .and(warp::path!("rooms" / i64 / "join"))
            .and(warp::post())
            .and_then(join_room)
            .with(warp::trace::named("join_room"));

        let start_vote = with_state(conn.clone())
            .and(with_state(broadcasters.clone()))
            .and(warp::path!("rooms" / i64 / "start"))
            .and(warp::put())
            .and(warp::cookie::cookie(names::ROOM_ADMIN_COOKIE_NAME))
            .and_then(start_vote)
            .with(warp::trace::named("start_vote"));

        let end_vote = with_state(conn.clone())
            .and(with_state(broadcasters.clone()))
            .and(warp::path!("rooms" / i64 / "end"))
            .and(warp::put())
            .and(warp::cookie::cookie(names::ROOM_ADMIN_COOKIE_NAME))
            .and_then(end_vote)
            .with(warp::trace::named("start_vote"));

        create_room
            .or(get_room)
            .or(join_room_page)
            .or(join_room)
            .or(start_vote)
            .or(end_vote)
    }

    async fn create_room(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
        mut body: CreateRoomBody,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        if body.name.is_empty() {
            return Err(warp::reject::custom(EmptyName));
        }

        if body.options.is_empty() {
            return Err(warp::reject::custom(NoOptions));
        }

        for opt in &body.options {
            if opt.is_empty() {
                return Err(warp::reject::custom(EmptyOption));
            }
        }

        body.options.sort();
        let options = serde_json::to_string(&body.options).unwrap();
        let admin_code = utils::generate_ulid();

        let room_id = sqlx::query!(
            r#"
        INSERT INTO rooms (name, options, admin_code)
        VALUES ( ?1, ?2, ?3 )
            "#,
            body.name,
            options,
            admin_code
        )
        .execute(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while creating room: {e}");
            warp::reject::custom(rejections::InternalServerError)
        })?
        .last_insert_rowid();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;

            let res = sqlx::query!(
                r#"
            BEGIN TRANSACTION;

            DELETE FROM voters
            WHERE room_id = ?1;

            DELETE FROM rooms
            WHERE id = ?1;

            COMMIT;
                "#,
                room_id,
                room_id,
            )
            .execute(&conn)
            .await;
            tracing::debug!("delete room result: {res:?}");

            broadcasters.end_stream(room_id).await;
        });

        let set_cookie_value = format!(
            "{}={admin_code}; HttpOnly; Max-Age=3600; Secure; Path=/",
            names::ROOM_ADMIN_COOKIE_NAME
        );

        let resp = Response::builder()
            .header(SET_COOKIE, set_cookie_value)
            .header("HX-Replace-Url", names::room_page_url(room_id))
            .body(
                views::titled(
                    "Admin",
                    view(RoomPage {
                        id: room_id,
                        name: body.name,
                        options: body.options,
                        voters: Vec::new(),
                    }),
                )
                .into_string(),
            )
            .unwrap();

        Ok(resp)
    }

    async fn get_room(
        conn: sqlx::Pool<sqlx::Sqlite>,
        room_id: i64,
        admin_code: String,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT id, name, options, admin_code
        FROM rooms
        WHERE id = ?1 AND status = 0
            "#,
            room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting room: {e}");
            warp::reject::custom(rejections::InternalServerError)
        })?;

        let voters = sqlx::query!(
            r#"
        SELECT id, approved
        FROM voters
        WHERE room_id = ?1
            "#,
            room.id
        )
        .fetch_all(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting voters: {e}");
            warp::reject::custom(rejections::InternalServerError)
        })?;

        if room.admin_code != admin_code {
            return Err(warp::reject::custom(NotRoomAdmin));
        }

        let page = RoomPage {
            id: room.id,
            name: room.name,
            options: serde_json::from_str::<Vec<String>>(&room.options).unwrap(),
            voters: voters
                .into_iter()
                .map(|r| Voter {
                    id: r.id,
                    approved: r.approved,
                })
                .collect(),
        };

        Ok(views::page("Admin", view(page)))
    }

    struct RoomPage {
        id: i64,
        name: String,
        options: Vec<String>,
        voters: Vec<Voter>,
    }

    struct Voter {
        id: i64,
        approved: bool,
    }

    fn view(room: RoomPage) -> Markup {
        let voter_count = utils::format_num(room.voters.len() as i32);
        let voter_label = utils::pluralize(room.voters.len() as i32, "voter", "voters");

        html! {
            section."grid gap-lg w-800" hx-ext="sse" sse-connect=(names::room_listen_url(room.id)) {
                h1."text-lg" { (room.name) }

                div."alert" { "ROOM WILL CLOSE IN LESS THAN AN HOUR." }

                section."two-cols" {
                    div."card card--secondary stat" hx-swap="innerHTML" sse-swap=(names::VOTER_COUNT_EVENT){
                        p."stat__num" { (voter_count) }
                        p."stat__desc" { (voter_label) " in room" }
                    }

                    div."card grid gap-lg" {
                        h2."text-md" { "Options" }
                        div."grid gap-sm" {
                            @for option in room.options {
                                span."boxed" { (option) }
                            }
                        }
                    }
                }

                button."button text-lg align-left" hx-put=(names::start_vote_url(room.id)) hx-target="main" hx-swap="innerHTML" { "START VOTE" }

                section."grid gap-md" hx-swap="beforeend" sse-swap=(names::NEW_VOTER_EVENT) {
                    h2."text-md" { "VOTERS" }

                    span."strech code" {
                        span { "NEW VOTER LINK" }
                        span { "/rooms/" (room.id) "/join" }
                    }

                    @for voter in room.voters {
                        div."flex gap-md" {
                            span."strech code" {
                                span { "VOTER ID" }
                                span { (voter.id) }
                            }

                            @if voter.approved {
                                button."button w-fit" disabled { "APPROVED" }
                            } @else {
                                button."button w-fit" hx-put=(names::approve_voter_url(voter.id)) hx-swap="outerHTML" { "APPROVE" }
                            }
                        }
                    }
                }
            }
        }
    }

    async fn join_room_page(
        conn: sqlx::Pool<sqlx::Sqlite>,
        room_id: i64,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT name
        FROM rooms
        WHERE id = ?1 AND status = 0
            "#,
            room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting room name: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        Ok(views::page(
            &room.name,
            html! {
                section."grid gap-lg w-800" {
                    h1."text-lg" { "JOIN THE \"" (room.name) "\" ROOM" }
                    button."button w-full align-left" hx-post=(names::join_room_url(room_id)) hx-target="main" hx-swap="innerHTML" {
                        "JOIN ROOM"
                    }
                }
            },
        ))
    }

    async fn join_room(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
        room_id: i64,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room_name = sqlx::query!(
            r#"
        SELECT name
        FROM rooms
        WHERE id = ?1
            "#,
            room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting room: {e}");
            warp::reject::custom(InternalServerError)
        })?
        .name;

        let voter_code = utils::generate_ulid();
        let voter_id = sqlx::query!(
            r#"
        INSERT INTO voters (voter_code, room_id)
        VALUES (?1, ?2)
            "#,
            voter_code,
            room_id
        )
        .execute(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while creating new voter: {e}");
            warp::reject::custom(InternalServerError)
        })?
        .last_insert_rowid();

        let voter_count = sqlx::query!(
            "SELECT count(id) as count FROM voters WHERE room_id = ?1",
            room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting voters count: {e}");
            warp::reject::custom(InternalServerError)
        })?
        .count;

        tokio::spawn(async move {
            broadcasters
                .send_event(room_id, RoomEvents::NewVoterCount(voter_count))
                .await;
            broadcasters
                .send_event(room_id, RoomEvents::NewVoter(voter_id))
                .await;
        });

        let page = views::titled(
            "Voter",
            voters::view(VoterPage {
                id: voter_id,
                room_id,
                room_name,
                voter_count,
                approved: false,
            }),
        );
        let set_cookie_value = format!(
            "{}={voter_code}; HttpOnly; Max-Age=3600; Secure; Path=/",
            names::VOTER_COOKIE_NAME
        );

        let resp = Response::builder()
            .header(SET_COOKIE, set_cookie_value)
            .header("HX-Replace-Url", names::voter_page_url(voter_id))
            .body(page.into_string())
            .unwrap();

        Ok(resp)
    }

    async fn start_vote(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
        room_id: i64,
        admin_code: String,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT admin_code, name, options
        FROM rooms
        WHERE id = ?1 AND status = 0
            "#,
            room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting room: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        if admin_code != room.admin_code {
            return Err(warp::reject::custom(NotRoomAdmin));
        }

        sqlx::query!(
            r#"
        UPDATE rooms
        SET status = 1
        WHERE id = ?1
            "#,
            room_id
        )
        .execute(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while setting room status to `started`: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        let voters = sqlx::query!(
            r#"
        SELECT id, options
        FROM voters
        WHERE voters.room_id = ?1 AND voters.approved = TRUE
            "#,
            room_id
        )
        .fetch_all(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting voters: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        let options = serde_json::from_str(&room.options).unwrap();
        tokio::spawn(async move {
            broadcasters
                .send_event(room_id, RoomEvents::VoteStarted(options))
                .await;
        });

        let page = voting::admin_page(VoteAdminPage {
            room_id,
            room_name: room.name,
            recorded_votes: 0,
            approved_voters: voters
                .into_iter()
                .map(|v| voting::Voter {
                    id: v.id,
                    voted: v.options.map(|_| true).unwrap_or_default(),
                })
                .collect(),
        });

        Ok(views::titled("Vote Started", page))
    }

    async fn end_vote(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
        room_id: i64,
        admin_code: String,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT admin_code, name, options
        FROM rooms
        WHERE id = ?1 AND status = 1
            "#,
            room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting room: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        if admin_code != room.admin_code {
            return Err(warp::reject::custom(NotRoomAdmin));
        }

        sqlx::query!(
            r#"
        UPDATE rooms
        SET status = 2
        WHERE id = ?1
            "#,
            room_id
        )
        .execute(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while setting room status to `ended`: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        let votes = sqlx::query!(
            r#"
        SELECT options
        FROM voters
        WHERE voters.room_id = ?1 AND voters.approved = TRUE AND options NOT NULL
            "#,
            room_id
        )
        .fetch_all(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting voters: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        let scores = votes
            .into_iter()
            .map(|r| r.options.unwrap())
            .map(|r| serde_json::from_str::<Vec<String>>(&r).unwrap())
            .fold(HashMap::<String, usize>::new(), |map, options| {
                let options_len = options.len();
                options
                    .into_iter()
                    .enumerate()
                    .fold(map, |mut map, (idx, choice)| {
                        let curr_score = options_len - idx;
                        map.entry(choice)
                            .and_modify(|score| *score += curr_score)
                            .or_insert(curr_score);
                        map
                    })
            });

        let mut scores = scores.into_iter().collect::<Vec<_>>();
        scores.sort_by_key(|(_, score)| *score);
        scores.reverse();

        tokio::spawn(async move {
            broadcasters
                .send_event(room_id, RoomEvents::VoteEnded)
                .await;
            broadcasters.end_stream(room_id).await;
        });

        let page = voting::result_page(ResultPage {
            room_name: room.name,
            scores: scores
                .into_iter()
                .map(|(option, score)| Score { option, score })
                .collect(),
        });

        Ok(views::titled("Vote Ended", page))
    }
}

mod voters {
    use maud::{html, Markup};
    use serde::Deserialize;
    use warp::Filter;

    use crate::{
        events::{Broadcasters, RoomEvents},
        names,
        rejections::{InternalServerError, NotRoomAdmin, NotVoter, UnknownOptions},
        utils, views, with_state,
    };

    #[derive(Deserialize)]
    struct VoteBody {
        options: Vec<String>,
    }

    pub fn route(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
    ) -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let get_voter = with_state(conn.clone())
            .and(warp::path!("voters" / i64))
            .and(warp::get())
            .and(warp::cookie::cookie(names::VOTER_COOKIE_NAME))
            .and_then(get_voter)
            .with(warp::trace::named("get_voter"));

        let approve_voter = with_state(conn.clone())
            .and(with_state(broadcasters.clone()))
            .and(warp::path!("voters" / i64 / "approve"))
            .and(warp::put())
            .and(warp::cookie::cookie(names::ROOM_ADMIN_COOKIE_NAME))
            .and_then(approve_voter)
            .with(warp::trace::named("approve_voter"));

        let vote = with_state(conn.clone())
            .and(with_state(broadcasters.clone()))
            .and(warp::path!("voters" / i64 / "vote"))
            .and(warp::post())
            .and(warp::cookie::cookie(names::VOTER_COOKIE_NAME))
            .and(warp::body::json::<VoteBody>())
            .and_then(vote)
            .with(warp::trace::named("vote"));

        get_voter.or(approve_voter).or(vote)
    }

    async fn get_voter(
        conn: sqlx::Pool<sqlx::Sqlite>,
        voter_id: i64,
        voter_code: String,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let voter = sqlx::query!(
            r#"
        SELECT voter_code, approved, room_id
        FROM voters
        WHERE id = ?1
            "#,
            voter_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting voter: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        if voter_code != voter.voter_code {
            return Err(warp::reject::custom(NotVoter));
        }

        let room_name = sqlx::query!(
            r#"
        SELECT name
        FROM rooms
        WHERE id = ?1 AND status = 0
            "#,
            voter.room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting room: {e}");
            warp::reject::custom(InternalServerError)
        })?
        .name;

        let voter_count = sqlx::query!(
            r#"
        SELECT count(id) as count
        FROM voters
        WHERE room_id = ?1
            "#,
            voter.room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting voter count: {e}");
            warp::reject::custom(InternalServerError)
        })?
        .count;

        Ok(views::page(
            "Voter",
            view(VoterPage {
                id: voter_id,
                room_id: voter.room_id,
                room_name,
                voter_count,
                approved: voter.approved,
            }),
        ))
    }

    pub struct VoterPage {
        pub id: i64,
        pub room_id: i64,
        pub room_name: String,
        pub voter_count: i32,
        pub approved: bool,
    }

    pub fn view(voter: VoterPage) -> Markup {
        let voter_count = utils::format_num(voter.voter_count);
        let voter_label = utils::pluralize(voter.voter_count, "voter", "voters");

        html! {
            section."grid gap-lg w-800" hx-ext="sse" sse-connect=(names::room_listen_url(voter.room_id)) {
                h1."text-lg" { (voter.room_name) }

                section."two-cols" {
                    div."card card--secondary stat" {
                        p."stat__num" hx-swap="innerHTML" sse-swap=(names::VOTER_COUNT_EVENT) { (voter_count) }
                        p."stat__desc" { (voter_label) " in room" }
                    }

                    div."card grid gap-lg" {
                        h2."text-md" { "YOUR VOTER ID" }
                        span."code" { (voter.id) }
                        @if voter.approved {
                            div."alert" { "VOTER HAS BEEN APPROVED." }
                        } else {
                            div."alert" hx-swap="outerHTML" sse-swap=(names::voter_approved_event(voter.id)) {
                                "WAITING TO BE APPROVED."
                            }
                        }
                    }
                }

                div hx-swap="innerHTML" sse-swap=(names::VOTE_STARTED_EVENT) {
                    div."alert" { "VOTES WILL START SHORTLY." }
                }

                div hx-swap="innerHTML" sse-swap=(names::VOTE_ENDED_EVENT) { }
            }
        }
    }

    async fn approve_voter(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
        voter_id: i64,
        admin_code: String,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT id, admin_code
        FROM rooms
        WHERE id = (SELECT room_id FROM voters WHERE id = ?1)
            "#,
            voter_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting voter: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        if admin_code != room.admin_code {
            return Err(warp::reject::custom(NotRoomAdmin));
        }

        sqlx::query!(
            r#"UPDATE voters SET approved = true WHERE id = ?1"#,
            voter_id
        )
        .execute(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while approving voter: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        tokio::spawn(async move {
            broadcasters
                .send_event(room.id, RoomEvents::VoterApproved(voter_id))
                .await;
        });

        Ok(html! {
            button."button w-fit" disabled { "APPROVED" }
        })
    }

    async fn vote(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
        voter_id: i64,
        voter_code: String,
        body: VoteBody,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let voter = sqlx::query!(
            r#"
        SELECT voter_code, approved, room_id
        FROM voters
        WHERE id = ?1
            "#,
            voter_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting voter: {e}");
            warp::reject::custom(InternalServerError)
        })?;

        if voter_code != voter.voter_code {
            return Err(warp::reject::custom(NotVoter));
        }

        let room_options = sqlx::query!(
            r#"
        SELECT options
        FROM rooms
        WHERE id = ?1 AND status = 1
            "#,
            voter.room_id
        )
        .fetch_one(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while getting room: {e}");
            warp::reject::custom(InternalServerError)
        })?
        .options;

        let room_options: Vec<String> = serde_json::from_str(&room_options).unwrap();
        let mut voter_options = body.options.clone();
        voter_options.sort();

        if room_options != voter_options {
            return Err(warp::reject::custom(UnknownOptions));
        }

        let options = serde_json::to_string(&body.options).unwrap();

        let _ = sqlx::query!(
            r#"
        UPDATE voters
        SET options = ?1
        WHERE id = ?2
            "#,
            options,
            voter_id
        )
        .execute(&conn)
        .await
        .map_err(|e| {
            tracing::error!("error while storing vote options: {e}");
            warp::reject::custom(InternalServerError)
        });

        tokio::spawn(async move {
            broadcasters
                .send_event(voter.room_id, RoomEvents::NewVote(voter_id))
                .await;

            if let Ok(votes) = sqlx::query!(
                r#"
            SELECT count(id) as count
            FROM voters
            WHERE room_id = ?1 AND options NOT NULL
                "#,
                voter.room_id
            )
            .fetch_one(&conn)
            .await
            .map(|r| r.count)
            {
                broadcasters
                    .send_event(voter.room_id, RoomEvents::NewVoteCount(votes))
                    .await;
            }
        });

        Ok(html! {
            h2."text-md" { "THANKS FOR VOTING!" }
        })
    }
}

mod voting {
    use maud::{html, Markup};

    use crate::{names, utils};

    pub struct VoteAdminPage {
        pub room_id: i64,
        pub room_name: String,
        pub recorded_votes: i32,
        pub approved_voters: Vec<Voter>,
    }

    pub struct Voter {
        pub id: i64,
        pub voted: bool,
    }

    pub fn admin_page(page: VoteAdminPage) -> Markup {
        let approved_count = utils::format_num(page.approved_voters.len() as i32);
        let approved_label = utils::pluralize(page.approved_voters.len() as i32, "voter", "voters");

        let recorded_votes = utils::format_num(page.recorded_votes);
        let recorded_votes_label = utils::pluralize(page.recorded_votes, "vote", "votes");

        html! {
            section."grid gap-lg w-800" hx-ext="sse" sse-connect=(names::room_listen_url(page.room_id)) {
                h1."text-lg" { (page.room_name) }

                div."alert" { "ROOM WILL CLOSE IN LESS THAN AN HOUR." }

                section."two-cols" {
                    div."card card--secondary stat" {
                        p."stat__num" { (approved_count) }
                        p."stat__desc" { "approved " (approved_label) }
                    }

                    div."card stat" hx-swap="innerHTML" sse-swap=(names::VOTE_COUNT_EVENT) {
                        p."stat__num" { (recorded_votes) }
                        p."stat__desc" { "recorded " (recorded_votes_label) }
                    }
                }

                button."button text-lg align-left" hx-put=(names::end_vote_url(page.room_id)) hx-target="main" hx-swap="innerHTML" { "END VOTE" }

                section."grid gap-md" {
                    h2."text-md" { "APPROVED VOTERS" }

                    @for voter in page.approved_voters {
                        div."flex gap-md" {
                            span."strech code" {
                                span { "VOTER ID" }
                                span { (voter.id) }
                            }

                            @if voter.voted {
                                span."boxed" { "VOTED" }
                            } @else {
                                span."boxed" sse-swap=(names::vote_event(voter.id)) hx-swap="outerHTML" { "WAITING" }
                            }
                        }
                    }
                }
            }
        }
    }

    pub struct ResultPage {
        pub room_name: String,
        pub scores: Vec<Score>,
    }

    pub struct Score {
        pub option: String,
        pub score: usize,
    }

    pub fn result_page(page: ResultPage) -> Markup {
        html! {
            section."grid gap-lg w-800" {
                h1."text-lg" { "RESULTS FOR \"" (page.room_name) "\"" }

                section."grid gap-sm" {
                    div."big-small gap-sm" {
                        p."code text-sm" { "OPTION" }
                        p."code text-sm" { "SCORE" }
                    }

                    @for score in page.scores {
                        div."big-small gap-sm" {
                            div."card" {
                                p."text-sm" { (score.option) }
                            }

                            div."card card--secondary" {
                                p."text-sm" { (utils::format_num(score.score as i32)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

mod events {
    use std::{collections::HashMap, convert::Infallible, sync::Arc};

    use maud::html;
    use tokio::sync::{
        broadcast::{self, Sender},
        Mutex,
    };
    use tokio_stream::{wrappers::BroadcastStream, StreamExt};
    use warp::{
        filters::sse::{self, Event},
        Filter,
    };

    use crate::{names, rejections::InternalServerError, utils, with_state};

    #[derive(Clone, Debug)]
    pub enum RoomEvents {
        NewVoter(i64),
        NewVoterCount(i32),
        VoterApproved(i64),
        VoteStarted(Vec<String>),
        VoteEnded,
        NewVote(i64),
        NewVoteCount(i32),
    }

    #[derive(Clone, Default)]
    pub struct Broadcasters {
        map: Arc<Mutex<HashMap<i64, Sender<RoomEvents>>>>,
    }

    impl Broadcasters {
        pub fn new() -> Self {
            Default::default()
        }

        pub async fn send_event(&self, room_id: i64, event: RoomEvents) {
            let mut map = self.map.lock().await;
            let tx = map
                .entry(room_id)
                .or_insert_with(|| broadcast::channel(16).0);

            let res = tx.send(event);
            tracing::debug!("send event result: {res:?}");
        }

        async fn get_stream(&self, room_id: i64) -> BroadcastStream<RoomEvents> {
            let mut map = self.map.lock().await;
            let tx = map
                .entry(room_id)
                .or_insert_with(|| broadcast::channel(16).0);
            let rx = tx.subscribe();

            BroadcastStream::new(rx)
        }

        pub async fn end_stream(&self, room_id: i64) {
            let mut map = self.map.lock().await;
            let res = map.remove(&room_id);
            tracing::debug!("end stream result: {res:?}");
        }
    }

    pub fn route(
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
    ) -> impl warp::Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::path!("rooms" / i64 / "listen")
            .and(with_state(conn))
            .and(with_state(broadcasters))
            .and(warp::cookie::optional(names::ROOM_ADMIN_COOKIE_NAME))
            .and(warp::cookie::optional(names::VOTER_COOKIE_NAME))
            .and_then(handler)
    }

    async fn handler(
        room_id: i64,
        conn: sqlx::Pool<sqlx::Sqlite>,
        broadcasters: Broadcasters,
        admin_code: Option<String>,
        voter_code: Option<String>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let admin = match admin_code {
            Some(admin_code) => {
                let room = sqlx::query!(
                    r#"
                SELECT id, admin_code
                FROM rooms
                WHERE id = ?1
                    "#,
                    room_id
                )
                .fetch_one(&conn)
                .await
                .map_err(|e| {
                    tracing::error!("error while getting admin code: {e}");
                    warp::reject::custom(InternalServerError)
                })?;

                if admin_code == room.admin_code {
                    Some(room.id)
                } else {
                    None
                }
            }
            None => None,
        };

        let voter = match voter_code {
            Some(voter_code) => sqlx::query!(
                r#"
            SELECT id
            FROM voters
            WHERE voter_code = ?1
                "#,
                voter_code
            )
            .fetch_optional(&conn)
            .await
            .map_err(|e| {
                tracing::error!("error while getting admin code: {e}");
                warp::reject::custom(InternalServerError)
            })?
            .map(|v| v.id),
            None => None,
        };

        let stream = broadcasters.get_stream(room_id).await;
        let stream = stream
            .filter_map(|event| match event {
                Ok(event) => Some(event),
                Err(error) => {
                    tracing::error!("error while receiving events: {error}");
                    None
                }
            })
            .map(move |event| {
                use RoomEvents::*;
                match (event, admin, voter) {
                    (NewVoterCount(count), Some(_), None) | (NewVoterCount(count), None, Some(_)) => {
                        Event::default()
                            .event(names::VOTER_COUNT_EVENT)
                            .data(html! {
                                p."stat__num" { (utils::format_num(count)) }
                                p."stat__desc" { (utils::pluralize(count, "voter", "voters")) " in room" }
                            }.into_string())
                    }

                    (NewVoter(voter_id), Some(_), None) => Event::default()
                        .event(names::NEW_VOTER_EVENT)
                        .data(html! {
                            div."flex gap-md" {
                                span."strech code" {
                                    span { "VOTER ID" }
                                    span { (voter_id) }
                                }
                                button."button w-fit" hx-put=(names::approve_voter_url(voter_id)) hx-swap="outerHTML" { "APPROVE" }
                            }
                        }.into_string()),

                    (VoterApproved(voter_id), Some(_), None) => Event::default()
                        .event(names::voter_approved_event(voter_id))
                        .data(html! {
                            button."button w-fit" disabled { "APPROVED" }
                        }.into_string()),

                    (VoterApproved(voter_id), None, Some(listener)) if voter_id == listener => Event::default()
                        .event(names::voter_approved_event(voter_id))
                        .data(html! {
                            div."alert" { "VOTER HAS BEEN APPROVED." }
                        }.into_string()),

                    (VoteStarted(options), None, Some(voter_id)) => Event::default()
                        .event(names::VOTE_STARTED_EVENT)
                        .data(html! {
                            form."grid gap-md sortable" hx-ext="json-enc" hx-post=(names::vote_url(voter_id)) hx-swap="outerHTML" {
                                h2."text-lg" { "START VOTING" }
                                p."text-sm" { "(REORDER THE OPTIONS BY DRAGGING AND DROPPING THEM)" }

                                div."grid gap-md sortable" {
                                    @for option in options {
                                        div."card" {
                                            (option)
                                            input type="hidden" name="options" value=(option) {}
                                        }
                                    }
                                }

                                button."button align-left" type="submit" { "SUBMIT VOTE" }
                            }
                        }.into_string()),

                    (NewVote(voter_id), Some(_), None) => Event::default()
                        .event(names::vote_event(voter_id))
                        .data(html! {
                            span."boxed" { "VOTED" }
                        }.into_string()),

                    (NewVoteCount(votes), Some(_), None) => Event::default()
                        .event(names::VOTE_COUNT_EVENT)
                        .data(html! {
                            p."stat__num" { (utils::format_num(votes)) }
                            p."stat__desc" { "recorded " (utils::pluralize(votes, "vote", "votes")) }
                        }.into_string()),

                    (VoteEnded, None, Some(_)) => Event::default()
                        .event(names::VOTE_ENDED_EVENT)
                        .data(html! { div."alert" { "VOTES HAVE ENDED." } }.into_string()),

                    _ => Event::default().event(names::PING_EVENT),
                }
            })
            .map(Ok::<_, Infallible>);

        Ok(sse::reply(stream))
    }
}

mod utils {
    use num_format::{Locale, ToFormattedString};
    use ulid::Ulid;

    pub fn format_num(num: i32) -> String {
        num.to_formatted_string(&Locale::en)
    }

    pub fn pluralize(num: i32, singular: &str, plural: &str) -> String {
        if num == 1 { singular } else { plural }.to_owned()
    }

    pub fn generate_ulid() -> String {
        Ulid::new().to_string()
    }
}

mod names {
    pub fn rooms_url() -> String {
        "/rooms".to_owned()
    }

    pub fn room_page_url(room_id: i64) -> String {
        format!("/rooms/{room_id}")
    }

    pub fn start_vote_url(room_id: i64) -> String {
        format!("/rooms/{room_id}/start")
    }

    pub fn end_vote_url(room_id: i64) -> String {
        format!("/rooms/{room_id}/end")
    }

    pub fn room_listen_url(room_id: i64) -> String {
        format!("/rooms/{room_id}/listen")
    }

    pub fn join_room_url(room_id: i64) -> String {
        format!("/rooms/{room_id}/join")
    }

    pub fn voter_page_url(voter_id: i64) -> String {
        format!("/voters/{voter_id}")
    }

    pub fn approve_voter_url(voter_id: i64) -> String {
        format!("/voters/{voter_id}/approve")
    }

    pub fn vote_url(voter_id: i64) -> String {
        format!("/voters/{voter_id}/vote")
    }

    pub const VOTER_COUNT_EVENT: &str = "voter-count";
    pub const NEW_VOTER_EVENT: &str = "voter";

    pub const VOTE_STARTED_EVENT: &str = "vote-started";
    pub const VOTE_ENDED_EVENT: &str = "vote-ended";
    pub const VOTE_COUNT_EVENT: &str = "vote-count";

    pub const PING_EVENT: &str = "ping";

    pub fn voter_approved_event(voter_id: i64) -> String {
        format!("voter-approved:{voter_id}")
    }

    pub fn vote_event(voter_id: i64) -> String {
        format!("vote:{voter_id}")
    }

    pub const ROOM_ADMIN_COOKIE_NAME: &str = "admin_code";
    pub const VOTER_COOKIE_NAME: &str = "voter_code";
}

mod views {
    use maud::{html, Markup, PreEscaped, DOCTYPE};

    fn font() -> Markup {
        html! {
            link rel="preconnect" href="https://fonts.googleapis.com";
            link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
            link href="https://fonts.googleapis.com/css2?family=Darker+Grotesque:wght@300..900&display=swap" rel="stylesheet";
        }
    }

    fn htmx() -> Markup {
        html! {
            script src="https://unpkg.com/htmx.org@1.9.12" {}
            script src="https://unpkg.com/htmx.org@1.9.12/dist/ext/sse.js" {}
            script src="https://unpkg.com/htmx.org@1.9.12/dist/ext/json-enc.js" {}
            script src="https://unpkg.com/sortablejs@1.15.2" {}
        }
    }

    fn css() -> Markup {
        html! {
            link rel="stylesheet" href="/static/style.css";
        }
    }

    fn js() -> Markup {
        html! {
            script src="/static/main.js" {}
        }
    }

    fn header() -> Markup {
        html! {
            header."header" {
                a."header__logo" href="/" { "ORDO" }
            }
        }
    }

    fn main(body: Markup) -> Markup {
        html! {
            main."main" { (body) }
        }
    }

    pub fn page(title: &str, body: Markup) -> Markup {
        html! {
            (DOCTYPE)
            head {
                meta charset="utf-8";

                (font())
                (htmx())
                (css())
                (js())

                title { (format!("{title} - ORDO")) }
            }

            body {
                (header())
                (main(body))
            }
        }
    }

    pub fn titled(title: &str, body: Markup) -> Markup {
        html! {
            (body)
            (PreEscaped(format!("<script>document.title = `{title} - ORDO`;</script>")))
        }
    }
}

mod rejections {
    use std::convert::Infallible;

    use maud::{html, Markup};
    use warp::{
        http::StatusCode,
        reject::{Reject, Rejection},
        reply::Reply,
    };

    use crate::views;

    macro_rules! rejects {
        ($($name:ident),*) => {
            $(
                #[derive(Debug)]
                pub struct $name;

                impl Reject for $name {}
            )*
        };
    }

    rejects!(
        NotVoter,
        EmptyName,
        NoOptions,
        EmptyOption,
        NotRoomAdmin,
        UnknownOptions,
        InternalServerError
    );

    pub async fn handle_rejection(err: Rejection) -> Result<impl Reply, Infallible> {
        let code;
        let message;

        if err.is_not_found() {
            code = StatusCode::NOT_FOUND;
            message = "NOT_FOUND";
        } else if err
            .find::<warp::filters::body::BodyDeserializeError>()
            .is_some()
        {
            message = "BAD_REQUEST";
            code = StatusCode::BAD_REQUEST;
        } else if err.find::<warp::reject::MethodNotAllowed>().is_some() {
            code = StatusCode::METHOD_NOT_ALLOWED;
            message = "METHOD_NOT_ALLOWED";
        } else {
            tracing::error!("unhandled rejection: {:?}", err);
            code = StatusCode::INTERNAL_SERVER_ERROR;
            message = "UNHANDLED_REJECTION";
        }

        Ok(warp::reply::with_status(error_page(message), code))
    }

    fn error_page(message: &str) -> Markup {
        views::page(
            "Error",
            html! {
                div."error" {
                    div {
                        h1."bold" {"ERROR"}
                        p."regular" {(message)}
                    }
                    img src="/static/img/death.svg";
                }
            },
        )
    }
}
