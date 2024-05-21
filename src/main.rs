use std::env;

use sqlx::{Pool, Sqlite};
use tracing_subscriber::fmt::format::FmtSpan;
use warp::Filter;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "tracing=info,warp=debug".to_owned());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(FmtSpan::CLOSE)
        .init();

    let db = env::var("DATABASE_URL")?;
    let conn: Pool<Sqlite> = Pool::connect(&db).await?;

    let home = rooms::routes(conn);
    let static_files = warp::path("static").and(statics::routes());

    let routes = static_files.or(home).recover(rejections::handle_rejection);
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

mod views {
    use maud::{html, Markup, DOCTYPE};

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
                title { (format!("{title} - ORDO")) }
            }

            body {
                header { h1."logo" { a href="/" { "ORDO" } } }
                main { (body) }
            }
        }
    }
}

mod rooms {
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
            CouldNotStartVote, NotAnAdmin, OptionsMismatch, RoomNotFound, VoterNotFound,
            VoterNotInRoom, VotersNotFound,
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
                            h2."bold" { "Voters" }

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
        SET started = TRUE
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

                a."start bold" href={ "/rooms/" (room_vid) "/end" } { "End Vote" }
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

        let options = options.ok_or_else(|| warp::reject::not_found())?;
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

        let submit_vote = with_state(conn)
            .and(with_state(id_to_votes_tx.clone()))
            .and(warp::post())
            .and(warp::path!("voters" / String / "vote"))
            .and(warp::body::form::<Vec<(String, String)>>())
            .and_then(submit_vote);

        room_page
            .or(create_room)
            .or(homepage)
            .or(approve_voter)
            .or(voter_approved)
            .or(start_vote)
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
        CouldNotGetVoterCountStream
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
            eprintln!("unhandled rejection: {:?}", err);
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
