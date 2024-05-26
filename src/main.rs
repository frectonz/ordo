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
    let broadcasters = Broadcasters::new();

    let _home = old_rooms::routes(conn.clone());

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
        let rooms = sqlx::query!(r#"SELECT count(id) as count FROM new_rooms"#)
            .fetch_one(&conn)
            .await
            .map_err(|e| {
                tracing::error!("error while counting rooms: {e}");
                warp::reject::custom(rejections::InternalServerError)
            })?;

        let voters = sqlx::query!(r#"SELECT count(id) as count FROM new_voters"#)
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
    use std::time::Duration;

    use crate::{
        events::{Broadcasters, RoomEvents},
        names,
        rejections::{
            self, EmptyName, EmptyOption, InternalServerError, NoOptions, NotAnAdmin, NotRoomAdmin,
        },
        utils, views,
        voters::{self, VoterPage},
        voting::{self, VoteAdminPage},
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

        create_room
            .or(get_room)
            .or(join_room_page)
            .or(join_room)
            .or(start_vote)
    }

    async fn create_room(
        conn: sqlx::Pool<sqlx::Sqlite>,
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
        INSERT INTO new_rooms (name, options, admin_code)
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

            DELETE FROM new_voters
            WHERE room_id = ?1;

            DELETE FROM new_rooms
            WHERE id = ?1;

            COMMIT;
                "#,
                room_id,
                room_id,
            )
            .execute(&conn)
            .await;

            tracing::debug!("delete room result: {res:?}");
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
        FROM new_rooms
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
        FROM new_voters
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
        FROM new_rooms
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
        FROM new_rooms
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
        INSERT INTO new_voters (voter_code, room_id)
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
            "SELECT count(id) as count FROM new_voters WHERE room_id = ?1",
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
        FROM new_rooms
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
            return Err(warp::reject::custom(NotAnAdmin));
        }

        sqlx::query!(
            r#"
        UPDATE new_rooms
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
        FROM new_voters
        WHERE new_voters.room_id = ?1 AND new_voters.approved = TRUE
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
}

mod voters {
    use maud::{html, Markup};
    use serde::Deserialize;
    use warp::Filter;

    use crate::{
        events::{Broadcasters, RoomEvents},
        names,
        rejections::{InternalServerError, NotAnAdmin, NotVoter, UnknownOptions},
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
        FROM new_voters
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
        FROM new_rooms
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
        FROM new_voters
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
        FROM new_rooms
        WHERE id = (SELECT room_id FROM new_voters WHERE id = ?1)
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
            return Err(warp::reject::custom(NotAnAdmin));
        }

        sqlx::query!(
            r#"UPDATE new_voters SET approved = true WHERE id = ?1"#,
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
        FROM new_voters
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
        FROM new_rooms
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
        UPDATE new_voters
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
            FROM new_voters
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

            let _ = tx
                .send(event)
                .map_err(|e| tracing::error!("failed to send event: {e}"));
        }

        async fn get_stream(&self, room_id: i64) -> BroadcastStream<RoomEvents> {
            let mut map = self.map.lock().await;
            let tx = map
                .entry(room_id)
                .or_insert_with(|| broadcast::channel(16).0);
            let rx = tx.subscribe();

            BroadcastStream::new(rx)
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
                FROM new_rooms
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
            FROM new_voters
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

                    _ => Event::default().event(names::PING_EVENT),
                }
            })
            .map(|event| Ok::<_, Infallible>(event));

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
            link rel="stylesheet" href="/static/css/style.css";
        }
    }

    fn js() -> Markup {
        html! {
            script src="/static/js/main.js" {}
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

    pub fn with_layout(title: &str, head: Markup, body: Markup) -> Markup {
        html! {
            (DOCTYPE)
            head {
                meta charset="utf-8";

                link rel="preconnect" href="https://fonts.googleapis.com";
                link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
                link href="https://fonts.googleapis.com/css2?family=Darker+Grotesque:wght@300..900&display=swap" rel="stylesheet";

                link rel="stylesheet" href="/static/css/global.css";
                (head)
                title { (format!("ORDO - {title}")) }
            }

            body {
                header { h1."logo" { a href="/" { "ORDO" } } }
                main { (body) }
            }
        }
    }
}

mod old_rooms {
    use std::{
        collections::HashMap,
        convert::Infallible,
        sync::{Arc, Mutex},
    };

    use color_eyre::eyre::Result;
    use futures_util::StreamExt;
    use maud::{html, PreEscaped};
    use num_format::{Locale, ToFormattedString};
    use sqlx::{Pool, Sqlite};
    use tokio::sync::mpsc::{self, UnboundedSender};
    use tokio_stream::wrappers::UnboundedReceiverStream;
    use ulid::Ulid;
    use warp::{filters::sse, http::Uri, Filter};

    use crate::{
        rejections::{
            CouldNotApproveVoter, CouldNotCountVotes, CouldNotCreateNewRoom,
            CouldNotCreateNewVoter, CouldNotCreateVote, CouldNotDeserilizeOptions,
            CouldNotGetCount, CouldNotGetVoterCountStream, CouldNotGetVotersStream,
            CouldNotGetVotes, CouldNotStartVote, NotAnAdmin, OptionsMismatch, RoomNotFound,
            VoterNotFound, VoterNotInRoom, VotersNotFound,
        },
        views::with_layout,
        with_state,
    };

    async fn homepage(conn: Pool<Sqlite>) -> Result<impl warp::Reply, warp::Rejection> {
        let rooms = sqlx::query!("SELECT count(id) as count FROM rooms")
            .fetch_one(&conn)
            .await
            .map_err(|_| warp::reject::custom(CouldNotGetCount))?;
        let count = rooms.count.to_formatted_string(&Locale::en);

        let page = with_layout(
            "Home",
            html! {
                link rel="stylesheet" href="/static/css/home.css";
                script defer="true" src="/static/js/home.js" {}
            },
            html! {
                div {
                    form method="POST" {
                        div."field" {
                            label."bold" { "Name" }
                            input."regular" name="name" required="true" placeholder="My super cool vote" {}
                        }

                        div."field" {
                            label."bold" { "Options" }

                            div."options" {
                                div."option" {
                                    input."regular" name="option" required="true" placeholder="a choice" {}
                                    button."bold delete" type="button" { "delete" }
                                }
                                div."option" {
                                    input."regular" name="option" required="true" placeholder="a choice" {}
                                    button."bold delete" type="button" { "delete" }
                                }
                            }

                            button."bold" id="addOption" type="button" { "add option" }
                        }

                        button."bold submit" type="submit" { "create room" }
                    }

                    p."regular" {
                        span."bold" { (count) }
                        " rooms created so far"
                    }
                }
                div {
                    img src="/static/img/vote.svg";
                }
            },
        );

        Ok(page)
    }

    async fn create_room(
        conn: Pool<Sqlite>,
        body: Vec<(String, String)>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let (name, options) =
            body.into_iter()
                .fold((None, None), |(mut name, mut options), (key, value)| {
                    match key.as_str() {
                        "name" if name.is_none() => name = Some(value),
                        "option" => {
                            options = match options {
                                None => Some(vec![value]),
                                Some(mut opts) => {
                                    opts.push(value);
                                    Some(opts)
                                }
                            }
                        }
                        _ => {}
                    };

                    (name, options)
                });

        let (name, options) = match (name, options) {
            (Some(name), Some(options)) => (name, options),
            _ => return Err(warp::reject::not_found()),
        };
        let options =
            serde_json::to_string(&options).expect("failed to serialize `options` into json");

        let vid = Ulid::new().to_string();
        let admin_code = Ulid::new().to_string();

        sqlx::query!(
            r#"
        INSERT INTO rooms (vid, name, options, admin_code)
        VALUES ( ?1, ?2, ?3, ?4 )
            "#,
            vid,
            name,
            options,
            admin_code
        )
        .execute(&conn)
        .await
        .map_err(|_| warp::reject::custom(CouldNotCreateNewRoom))?;

        let uri = format!("/rooms/{vid}").parse::<Uri>().unwrap();

        Ok(warp::reply::with_header(
            warp::redirect(uri),
            "Set-Cookie",
            format!("admin_code={admin_code}; HttpOnly; Max-Age=3600; Path=/rooms/{vid}; Secure"),
        ))
    }

    async fn room_page(
        conn: Pool<Sqlite>,
        id_to_count_senders: CountStreamIdToSenders,
        id_to_voters_senders: VotersStreamIdToSenders,
        room_vid: String,
        admin_code: Option<String>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT id, name, options, admin_code
        FROM rooms 
        WHERE vid = ?1
            "#,
            room_vid,
        )
        .fetch_one(&conn)
        .await
        .map_err(|_| warp::reject::custom(RoomNotFound))?;

        let voters = sqlx::query!(
            r#"
        SELECT vid, approved
        FROM voters
        WHERE room_id = ?1
            "#,
            room.id
        )
        .fetch_all(&conn)
        .await
        .map_err(|_| warp::reject::custom(VotersNotFound))?;

        let is_admin = admin_code.map(|c| c == room.admin_code).unwrap_or(false);

        let options: Vec<String> = serde_json::from_str(&room.options)
            .map_err(|_| warp::reject::custom(CouldNotDeserilizeOptions))?;

        let voters_count = voters.len().to_formatted_string(&Locale::en);

        let page = if is_admin {
            with_layout(
                &format!("Admin - {}", room.name),
                html! {
                    link rel="stylesheet" href="/static/css/admin.css";
                    script src="https://unpkg.com/htmx.org@1.9.12" {}
                    script src="https://unpkg.com/htmx.org@1.9.12/dist/ext/sse.js" {}
                },
                html! {
                    h1."bold" { (room.name) }

                    div."warning regular" { "Room will close in less than an hour." }

                    section."combo" {
                        div {
                            p."stat-num bold"
                                hx-ext="sse"
                                sse-connect={ "/rooms/" (room_vid) "/count" }
                                hx-swap="innerHTML"
                                sse-swap="message" { (voters_count) }
                            p."stat-desc regular" { "voters in room"}
                        }

                        div {
                            h2."bold" { "Options" }
                            div."options" {
                                @for option in options {
                                    span."option regular" { (option) }
                                }
                            }
                        }
                    }

                    a."start bold" href={ "/rooms/" (room_vid) "/start" } { "Start Vote" }

                    section."voters"
                        hx-ext="sse"
                        sse-connect={ "/rooms/" (room_vid) "/voters" }
                        hx-swap="beforeend"
                        sse-swap="message" {
                            h2."bold" { "VOTERS" }

                            @for voter in voters {
                                div."voter" {
                                    span."regular" { (voter.vid) }

                                    @if voter.approved {
                                        button."approve bold" disabled { "Approved" }
                                    } @else {
                                        button."approve bold"
                                            hx-post={ "/rooms/" (room_vid) "/voters/" (voter.vid) "/approve" }
                                            hx-swap="outerHTML" { "Approve" }
                                    }
                                }
                            }
                    }
                },
            )
        } else {
            let voter_vid = Ulid::new().to_string();

            sqlx::query!(
                r#"
            INSERT INTO voters (vid, room_id)
            VALUES ( ?1, ?2 )
                "#,
                voter_vid,
                room.id
            )
            .execute(&conn)
            .await
            .map_err(|_| warp::reject::custom(CouldNotCreateNewVoter))?;

            let row = sqlx::query!(
                r#"
            SELECT count(id) as count
            FROM voters
            WHERE room_id = ?1
                "#,
                room.id
            )
            .fetch_one(&conn)
            .await
            .map_err(|_| warp::reject::custom(CouldNotCreateNewVoter))?;
            let count = row.count.to_formatted_string(&Locale::en);

            {
                let map = id_to_count_senders.lock().unwrap();
                let txs = map
                    .get(&room_vid)
                    .ok_or_else(|| warp::reject::custom(CouldNotGetVoterCountStream))?;

                for tx in txs {
                    let _ = tx.send(count.clone());
                }
            }

            {
                let map = id_to_voters_senders.lock().unwrap();
                let txs = map
                    .get(&room_vid)
                    .ok_or_else(|| warp::reject::custom(CouldNotGetVotersStream))?;

                let voter = html! {
                    div."voter" {
                        span."regular" { (voter_vid) }
                        button."approve bold"
                            hx-post={ "/rooms/" (room_vid) "/voters/" (voter_vid) "/approve" }
                            hx-swap="outerHTML"{ "Approve" }
                    }
                };

                for tx in txs {
                    let _ = tx.send(voter.clone());
                }
            }

            let sortable = r#"
            document.addEventListener('htmx:afterSwap', function() {
                const voteOptions = document.querySelector('.vote-options');

                if (voteOptions) {
                    new Sortable(voteOptions, {
                        animation: 150,
                        ghostClass: 'vote-option-active'
                    });
                }
            });
            "#;

            with_layout(
                &format!("Voter - {}", room.name),
                html! {
                    link rel="stylesheet" href="/static/css/admin.css";
                    script src="https://unpkg.com/htmx.org@1.9.12" {}
                    script src="https://unpkg.com/htmx.org@1.9.12/dist/ext/sse.js" {}
                    script src="https://unpkg.com/sortablejs@1.15.2" {}
                    script { (sortable) }
                },
                html! {
                    h1."bold" { (room.name) }

                    div."warning regular" { "Votes will start shortly." }

                    section."combo" {
                        div {
                            p."stat-num bold"
                                hx-ext="sse"
                                sse-connect={ "/rooms/" (room_vid) "/count" }
                                hx-swap="innerHTML"
                                sse-swap="message" { (count) }
                            p."stat-desc regular" { "voters in room"}
                        }

                        div {
                            h2."bold" { "Your voter ID" }
                            span."id" { (voter_vid) }
                            div."warning regular"
                                hx-get={ "/voters/" (voter_vid) "/approve" }
                                hx-swap="outerHTML"
                                hx-trigger="load delay:1s"
                                style="margin: 0; margin-top: 20px;" {
                                "Waiting to be approved."
                            }
                        }
                    }

                    div hx-ext="sse" sse-connect={ "/rooms/" (room_vid) "/listen" } {
                        div hx-get={ "/voters/" (voter_vid) "/vote" }
                            hx-trigger="sse:start"
                            hx-swap="outerHTML" {}
                    }
                },
            )
        };

        Ok(page)
    }

    async fn count_sse(
        room_vid: String,
        id_to_senders: CountStreamIdToSenders,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let (tx, rx) = mpsc::unbounded_channel();

        {
            let mut map = id_to_senders.lock().unwrap();
            let txs = map.entry(room_vid.clone()).or_default();
            txs.push(tx);
        }

        let stream = UnboundedReceiverStream::new(rx);
        let event_stream =
            stream.map(|count| Ok::<sse::Event, Infallible>(sse::Event::default().data(count)));

        Ok(warp::sse::reply(sse::keep_alive().stream(event_stream)))
    }

    async fn voter_sse(
        room_vid: String,
        id_to_senders: VotersStreamIdToSenders,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let (tx, rx) = mpsc::unbounded_channel();

        {
            let mut map = id_to_senders.lock().unwrap();
            let txs = map.entry(room_vid).or_default();
            txs.push(tx);
        }

        let stream = UnboundedReceiverStream::new(rx);
        let event_stream =
            stream.map(|count| Ok::<sse::Event, Infallible>(sse::Event::default().data(count)));

        Ok(warp::sse::reply(sse::keep_alive().stream(event_stream)))
    }

    async fn room_sse(
        room_vid: String,
        id_to_senders: RoomEventsIdToSenders,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let (tx, rx) = mpsc::unbounded_channel();

        {
            let mut map = id_to_senders.lock().unwrap();
            let txs = map.entry(room_vid).or_default();
            txs.push(tx);
        }

        let stream = UnboundedReceiverStream::new(rx);
        let event_stream = stream
            .map(|_| Ok::<sse::Event, Infallible>(sse::Event::default().event("start").data("")));

        Ok(warp::sse::reply(sse::keep_alive().stream(event_stream)))
    }

    async fn recorded_votes_sse(
        room_vid: String,
        id_to_votes: VotesIdToSenders,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let (tx, rx) = mpsc::unbounded_channel();

        {
            let mut map = id_to_votes.lock().unwrap();
            let txs = map.entry(room_vid.clone()).or_default();
            txs.push(tx);
        }

        let stream = UnboundedReceiverStream::new(rx);
        let event_stream =
            stream.map(|count| Ok::<sse::Event, Infallible>(sse::Event::default().data(count)));

        Ok(warp::sse::reply(sse::keep_alive().stream(event_stream)))
    }

    fn sse(
        id_to_count_tx: CountStreamIdToSenders,
        id_to_voter_tx: VotersStreamIdToSenders,
        id_to_room_tx: RoomEventsIdToSenders,
        id_to_votes: VotesIdToSenders,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let count_sse = warp::path!("rooms" / String / "count")
            .and(with_state(id_to_count_tx))
            .and_then(count_sse);

        let voter_sse = warp::path!("rooms" / String / "voters")
            .and(with_state(id_to_voter_tx))
            .and_then(voter_sse);

        let room_sse = warp::path!("rooms" / String / "listen")
            .and(with_state(id_to_room_tx))
            .and_then(room_sse);

        let recorded_votes_sse = warp::path!("rooms" / String / "votes")
            .and(with_state(id_to_votes))
            .and_then(recorded_votes_sse);

        count_sse.or(voter_sse).or(room_sse).or(recorded_votes_sse)
    }

    #[derive(Debug)]
    enum RoomEvents {
        Start,
        End,
    }

    type CountStreamIdToSenders = Arc<Mutex<HashMap<String, Vec<mpsc::UnboundedSender<String>>>>>;
    type VotersStreamIdToSenders =
        Arc<Mutex<HashMap<String, Vec<UnboundedSender<PreEscaped<String>>>>>>;
    type RoomEventsIdToSenders = Arc<Mutex<HashMap<String, Vec<UnboundedSender<RoomEvents>>>>>;
    type VotesIdToSenders = Arc<Mutex<HashMap<String, Vec<UnboundedSender<String>>>>>;

    async fn approve_voter(
        conn: Pool<Sqlite>,
        room_vid: String,
        voter_vid: String,
        admin_code: Option<String>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"SELECT id, admin_code FROM rooms WHERE vid = ?1"#,
            room_vid
        )
        .fetch_one(&conn)
        .await
        .map_err(|_| warp::reject::custom(RoomNotFound))?;

        let is_admin = admin_code.map(|c| c == room.admin_code).unwrap_or(false);
        if !is_admin {
            return Err(warp::reject::custom(NotAnAdmin));
        }

        let voter = sqlx::query!(r#"SELECT room_id FROM voters WHERE vid = ?1"#, voter_vid)
            .fetch_one(&conn)
            .await
            .map_err(|_| warp::reject::custom(RoomNotFound))?;

        let voter_in_room = room.id == voter.room_id;
        if !voter_in_room {
            return Err(warp::reject::custom(VoterNotInRoom));
        }

        sqlx::query!(
            r#"UPDATE voters SET approved = true WHERE vid = ?1"#,
            voter_vid
        )
        .execute(&conn)
        .await
        .map_err(|_| warp::reject::custom(CouldNotApproveVoter))?;

        let button = html! {
            button."approve bold" disabled
                hx-swap="outerHTML" { "Approved" }
        };

        Ok(button)
    }

    async fn voter_approved(
        conn: Pool<Sqlite>,
        voter_vid: String,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let voter = sqlx::query!(r#"SELECT approved FROM voters WHERE vid = ?1"#, voter_vid)
            .fetch_one(&conn)
            .await
            .map_err(|_| warp::reject::custom(VoterNotFound))?;

        let page = if voter.approved {
            html! {
                div."warning regular"
                    style="margin: 0; margin-top: 20px;" {
                    "You have been approved to enter the vote."
                }
            }
        } else {
            html! {
                div."warning regular"
                    hx-get={ "/voters/" (voter_vid) "/approve" }
                    hx-swap="outerHTML"
                    hx-trigger="load delay:1s"
                    style="margin: 0; margin-top: 20px;" {
                    "Waiting to be approved."
                }
            }
        };

        Ok(page)
    }

    async fn start_vote(
        conn: Pool<Sqlite>,
        id_to_room_tx: RoomEventsIdToSenders,
        room_vid: String,
        admin_code: Option<String>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT id, admin_code, name
        FROM rooms
        WHERE vid = ?1
            "#,
            room_vid
        )
        .fetch_one(&conn)
        .await
        .map_err(|_| warp::reject::custom(RoomNotFound))?;

        let is_admin = admin_code.map(|c| c == room.admin_code).unwrap_or(false);
        if !is_admin {
            return Err(warp::reject::custom(NotAnAdmin));
        }

        {
            let map = id_to_room_tx.lock().unwrap();
            let txs = map
                .get(&room_vid)
                .ok_or_else(|| warp::reject::custom(CouldNotGetVoterCountStream))?;

            for tx in txs {
                let _ = tx.send(RoomEvents::Start);
            }
        }

        sqlx::query!(
            r#"
        UPDATE rooms
        SET status = 1
        WHERE id = ?1
            "#,
            room.id
        )
        .execute(&conn)
        .await
        .map_err(|_| warp::reject::custom(CouldNotStartVote))?;

        let voters = sqlx::query!(
            r#"
        SELECT count(id) as count
        FROM voters 
        WHERE room_id = ?1 AND approved = true
            "#,
            room.id
        )
        .fetch_one(&conn)
        .await
        .map_err(|_| warp::reject::custom(VotersNotFound))?;
        let voters_count = voters.count.to_formatted_string(&Locale::en);

        let votes = sqlx::query!(
            r#"
        SELECT count(id) as count
        FROM votes
        WHERE room_id = ?1
            "#,
            room.id
        )
        .fetch_one(&conn)
        .await
        .map_err(|_| warp::reject::custom(CouldNotCountVotes))?;
        let votes_count = votes.count.to_formatted_string(&Locale::en);

        let page = with_layout(
            &format!("Receiving Votes - {}", room.name),
            html! {
                link rel="stylesheet" href="/static/css/admin.css";
                script src="https://unpkg.com/htmx.org@1.9.12" {}
                script src="https://unpkg.com/htmx.org@1.9.12/dist/ext/sse.js" {}
            },
            html! {
                h1."bold" { (room.name) }

                div."warning regular" { "Room will close in less than an hour." }

                section."combo" {
                    div {
                        p."stat-num bold" { (voters_count) }
                        p."stat-desc regular" { "approved voters"}
                    }

                    div {
                        p."stat-num bold"
                            hx-ext="sse"
                            sse-connect={ "/rooms/" (room_vid) "/votes" }
                            hx-swap="innerHTML"
                            sse-swap="message" { (votes_count) }
                        p."stat-desc regular" { "recorded votes" }
                    }
                }

                button."start bold" hx-post={ "/rooms/" (room_vid) "/end" } hx-swap="outerHTML" { "End Vote" }
            },
        );

        Ok(page)
    }

    async fn voting_page(
        conn: Pool<Sqlite>,
        voter_vid: String,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT options
        FROM rooms
        WHERE id = (SELECT room_id FROM voters WHERE vid = ?1)
            "#,
            voter_vid
        )
        .fetch_one(&conn)
        .await
        .map_err(|_| warp::reject::custom(RoomNotFound))?;

        let options: Vec<String> = serde_json::from_str(&room.options).unwrap();

        let page = html! {
            form."vote-options regular" hx-post={ "/voters/" (voter_vid) "/vote" } hx-swap="outerHTML" {
                @for option in options {
                    div."vote-option" {
                        (option)
                        input type="hidden" name="option" value=(option) {}
                    }
                }

                button."bold vote" type="submit" { "Submit Vote" }
            }
        };

        Ok(page)
    }

    async fn submit_vote(
        conn: Pool<Sqlite>,
        id_to_votes: VotesIdToSenders,
        voter_vid: String,
        body: Vec<(String, String)>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let options = body
            .into_iter()
            .fold(None, |options, (key, value)| match key.as_str() {
                "option" => match options {
                    None => Some(vec![value]),
                    Some(mut opts) => {
                        opts.push(value);
                        Some(opts)
                    }
                },
                _ => options,
            });

        let room = sqlx::query!(
            r#"
        SELECT id, vid, options
        FROM rooms
        WHERE id = (SELECT room_id FROM voters WHERE vid = ?1)
            "#,
            voter_vid
        )
        .fetch_one(&conn)
        .await
        .map_err(|_| warp::reject::custom(RoomNotFound))?;

        let mut the_options: Vec<String> = serde_json::from_str(&room.options).unwrap();
        the_options.sort();

        let options = options.ok_or_else(warp::reject::not_found)?;
        let mut user_options = options.clone();
        user_options.sort();

        if the_options != user_options {
            return Err(warp::reject::custom(OptionsMismatch));
        }

        let options = serde_json::to_string(&options).unwrap();

        sqlx::query!(
            r#"
        INSERT INTO votes (options, room_id, voter_id)
        SELECT ?1, ?2, id FROM voters WHERE vid = ?3
            "#,
            options,
            room.id,
            voter_vid
        )
        .execute(&conn)
        .await
        .map_err(|_| warp::reject::custom(CouldNotCreateVote))?;

        {
            let votes = sqlx::query!(
                r#"
            SELECT count(id) as count
            FROM votes
            WHERE room_id = ?1
            "#,
                room.id
            )
            .fetch_one(&conn)
            .await
            .map_err(|_| warp::reject::custom(CouldNotCountVotes))?;
            let votes_count = votes.count.to_formatted_string(&Locale::en);

            let map = id_to_votes.lock().unwrap();
            let txs = map
                .get(&room.vid)
                .ok_or_else(|| warp::reject::custom(CouldNotGetVoterCountStream))?;

            for tx in txs {
                let _ = tx.send(votes_count.clone());
            }
        }

        Ok(html! {
            h1."bold" style="margin: 1rem auto;" { "thanks for voting" }
        })
    }

    async fn end_vote(
        conn: Pool<Sqlite>,
        id_to_room_tx: RoomEventsIdToSenders,
        room_vid: String,
        admin_code: Option<String>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT id, admin_code, name
        FROM rooms
        WHERE vid = ?1
            "#,
            room_vid
        )
        .fetch_one(&conn)
        .await
        .map_err(|_| warp::reject::custom(RoomNotFound))?;

        let is_admin = admin_code.map(|c| c == room.admin_code).unwrap_or(false);
        if !is_admin {
            return Err(warp::reject::custom(NotAnAdmin));
        }

        let votes = sqlx::query!(
            r#"
        SELECT options
        FROM votes
        WHERE room_id = ?1
            "#,
            room.id
        )
        .fetch_all(&conn)
        .await
        .map_err(|_| warp::reject::custom(CouldNotGetVotes))?;

        let scores = votes
            .into_iter()
            .map(|r| r.options)
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

        {
            let map = id_to_room_tx.lock().unwrap();
            let txs = map
                .get(&room_vid)
                .ok_or_else(|| warp::reject::custom(CouldNotGetVoterCountStream))?;

            for tx in txs {
                let _ = tx.send(RoomEvents::End);
            }
        }

        let page = html! {
            table."regular" {
                thead."bold" {
                    tr {
                        td { "Option" }
                        td { "Score" }
                    }
                }
                tbody {
                    @for (option, score) in scores {
                        tr {
                            td { (option) }
                            td."bold" { (score.to_formatted_string(&Locale::en)) }
                        }
                    }
                }
            }
        };

        Ok(page)
    }

    pub fn routes(
        conn: Pool<Sqlite>,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let id_to_count_tx: CountStreamIdToSenders = Default::default();
        let id_to_voter_tx: VotersStreamIdToSenders = Default::default();
        let id_to_rooms_tx: RoomEventsIdToSenders = Default::default();
        let id_to_votes_tx: VotesIdToSenders = Default::default();

        let homepage = warp::path::end()
            .and(warp::get())
            .and(with_state(conn.clone()))
            .and_then(homepage);

        let create_room = warp::path::end()
            .and(warp::post())
            .and(with_state(conn.clone()))
            .and(warp::body::form::<Vec<(String, String)>>())
            .and_then(create_room);

        let room_page = with_state(conn.clone())
            .and(with_state(id_to_count_tx.clone()))
            .and(with_state(id_to_voter_tx.clone()))
            .and(warp::path!("rooms" / String))
            .and(warp::cookie::optional("admin_code"))
            .and_then(room_page);

        let approve_voter = with_state(conn.clone())
            .and(warp::path!(
                "rooms" / String / "voters" / String / "approve"
            ))
            .and(warp::cookie::optional("admin_code"))
            .and_then(approve_voter);

        let voter_approved = with_state(conn.clone())
            .and(warp::get())
            .and(warp::path!("voters" / String / "approve"))
            .and_then(voter_approved);

        let start_vote = warp::get()
            .and(with_state(conn.clone()))
            .and(with_state(id_to_rooms_tx.clone()))
            .and(warp::path!("rooms" / String / "start"))
            .and(warp::cookie::optional("admin_code"))
            .and_then(start_vote);

        let voting_page = with_state(conn.clone())
            .and(warp::get())
            .and(warp::path!("voters" / String / "vote"))
            .and_then(voting_page);

        let submit_vote = with_state(conn.clone())
            .and(with_state(id_to_votes_tx.clone()))
            .and(warp::post())
            .and(warp::path!("voters" / String / "vote"))
            .and(warp::body::form::<Vec<(String, String)>>())
            .and_then(submit_vote);

        let end_vote = warp::post()
            .and(with_state(conn.clone()))
            .and(with_state(id_to_rooms_tx.clone()))
            .and(warp::path!("rooms" / String / "end"))
            .and(warp::cookie::optional("admin_code"))
            .and_then(end_vote);

        room_page
            .or(create_room)
            .or(homepage)
            .or(approve_voter)
            .or(voter_approved)
            .or(start_vote)
            .or(end_vote)
            .or(voting_page)
            .or(submit_vote)
            .or(sse(
                id_to_count_tx,
                id_to_voter_tx,
                id_to_rooms_tx,
                id_to_votes_tx,
            ))
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

    use crate::views::with_layout;

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
        NotAnAdmin,
        RoomNotFound,
        VoterNotFound,
        VoterNotInRoom,
        VotersNotFound,
        OptionsMismatch,
        CouldNotGetVotes,
        CouldNotGetCount,
        CouldNotStartVote,
        CouldNotSendCount,
        CouldNotCountVotes,
        CouldNotCreateVote,
        CouldNotApproveVoter,
        CouldNotCreateNewRoom,
        CouldNotCreateNewVoter,
        CouldNotGetVoterCountTx,
        CouldNotGetVotersStream,
        CouldNotDeserilizeOptions,
        CouldNotGetVoterCountStream,
        UnknownOptions,
        NotVoter,
        NotRoomAdmin,
        EmptyName,
        NoOptions,
        EmptyOption,
        InternalServerError
    );

    pub async fn handle_rejection(err: Rejection) -> Result<impl Reply, Infallible> {
        let code;
        let message;

        if err.is_not_found() {
            code = StatusCode::NOT_FOUND;
            message = "NOT_FOUND";
        } else if let Some(NotAnAdmin) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "NOT_AN_ADMIN";
        } else if let Some(RoomNotFound) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "ROOM_NOT_FOUND";
        } else if let Some(VoterNotInRoom) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "VOTER_NOT_IN_ROOM";
        } else if let Some(VoterNotFound) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "VOTER_NOT_FOUND";
        } else if let Some(VotersNotFound) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "VOTERS_NOT_FOUND";
        } else if let Some(OptionsMismatch) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "OPTIONS_MISMATCH";
        } else if let Some(CouldNotGetVotes) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_GET_VOTES";
        } else if let Some(CouldNotGetCount) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_GET_COUNT";
        } else if let Some(CouldNotStartVote) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_START_VOTE";
        } else if let Some(CouldNotSendCount) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_SEND_COUNT";
        } else if let Some(CouldNotCountVotes) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_COUNT_VOTES";
        } else if let Some(CouldNotCreateVote) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_CREATE_VOTE";
        } else if let Some(CouldNotApproveVoter) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_APPROVE_VOTER";
        } else if let Some(CouldNotCreateNewRoom) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_CREATE_NEW_ROOM";
        } else if let Some(CouldNotCreateNewVoter) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_CREATE_NEW_VOTER";
        } else if let Some(CouldNotGetVoterCountTx) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_GET_VOTER_COUNT_TRANSMITTER";
        } else if let Some(CouldNotGetVotersStream) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_GET_VOTERS_STREAM";
        } else if let Some(CouldNotDeserilizeOptions) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_DESERILIZE_OPTIONS";
        } else if let Some(CouldNotGetVoterCountStream) = err.find() {
            code = StatusCode::BAD_REQUEST;
            message = "COULD_NOT_GET_VOTER_COUNT_STREAM";
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
        with_layout(
            "Error",
            html! {
                link rel="stylesheet" href="/static/css/error.css";
            },
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
