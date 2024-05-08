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

    let routes = static_files.or(home);
    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;

    Ok(())
}

fn with_db(
    db: Pool<Sqlite>,
) -> impl Filter<Extract = (Pool<Sqlite>,), Error = std::convert::Infallible> + Clone {
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
    use color_eyre::eyre::Result;
    use maud::html;
    use num_format::{Locale, ToFormattedString};
    use sqlx::{Pool, Sqlite};
    use ulid::Ulid;
    use warp::{http::Uri, Filter};

    use crate::{views::with_layout, with_db};

    async fn homepage(conn: Pool<Sqlite>) -> Result<impl warp::Reply, warp::Rejection> {
        let rooms = sqlx::query!("SELECT count(id) as count FROM rooms")
            .fetch_one(&conn)
            .await
            .or_else(|_| Err(warp::reject::not_found()))?;
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

                    return (name, options);
                });

        let (name, options) = match (name, options) {
            (Some(name), Some(options)) => (name, options),
            _ => return Err(warp::reject::not_found()),
        };
        let options =
            serde_json::to_string(&options).or_else(|_| Err(warp::reject::not_found()))?;

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
        .or_else(|_| Err(warp::reject::not_found()))?;

        let uri = format!("/rooms/{vid}").parse::<Uri>().unwrap();

        Ok(warp::reply::with_header(
            warp::redirect(uri),
            "Set-Cookie",
            format!("admin_code={admin_code}; HttpOnly; Max-Age=3600; Path=/rooms/{vid}; Secure"),
        ))
    }

    async fn room_page(
        conn: Pool<Sqlite>,
        vid: String,
        admin_code: Option<String>,
    ) -> Result<impl warp::Reply, warp::Rejection> {
        let room = sqlx::query!(
            r#"
        SELECT name, options, admin_code FROM rooms WHERE vid = ?1
            "#,
            vid,
        )
        .fetch_one(&conn)
        .await
        .or_else(|_| Err(warp::reject::not_found()))?;

        let is_admin = admin_code.map(|c| c == room.admin_code).unwrap_or(false);

        let page = with_layout(
            &room.name,
            html! {
                link rel="stylesheet" href="/static/css/room.css";
            },
            html! {
                h1."bold" {
                    @if is_admin {
                        "You are the admin."
                    } @else {
                        "You are not the admin."
                    }
                }
            },
        );

        Ok(page)
    }

    pub fn routes(
        conn: Pool<Sqlite>,
    ) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let homepage = warp::get().and(with_db(conn.clone())).and_then(homepage);

        let create_room = warp::post()
            .and(with_db(conn.clone()))
            .and(warp::body::form::<Vec<(String, String)>>())
            .and_then(create_room);

        let room_page = with_db(conn)
            .and(warp::path!("rooms" / String))
            .and(warp::cookie::optional("admin_code"))
            .and_then(room_page);

        room_page.or(create_room).or(homepage)
    }
}
