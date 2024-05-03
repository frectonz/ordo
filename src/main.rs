use sqlx::{Connection, SqliteConnection};
use tracing_subscriber::fmt::format::FmtSpan;
use warp::Filter;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "tracing=info,warp=debug".to_owned());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(FmtSpan::CLOSE)
        .init();

    let _conn = SqliteConnection::connect("sqlite::memory:").await?;

    let home = warp::path::end().and(rooms::routes());
    let static_files = warp::path("static").and(statics::routes());

    let routes = static_files.or(home);
    warp::serve(routes).run(([127, 0, 0, 1], 3030)).await;

    Ok(())
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
                link href="https://fonts.googleapis.com/css2?family=Gloria+Hallelujah&display=swap" rel="stylesheet";

                link rel="stylesheet" href="/static/css/global.css";
                (head)
                title { (format!("{title} - ORDO")) }
            }

            body {
                header { h1."hand" { a href="/" { "ORDO" } } }
                main { (body) }
            }
        }
    }
}

mod rooms {
    use maud::html;
    use warp::Filter;

    use crate::views::with_layout;

    fn homepage() -> impl warp::Reply {
        with_layout(
            "Home",
            html! {
                link rel="stylesheet" href="/static/css/home.css";
                script defer="true" src="/static/js/home.js" {}
            },
            html! {
                div {
                    form method="POST" {
                        div."field" {
                            label."hand" { "Name" }
                            input."hand" name="name" required="true" placeholder="My super cool vote" {}
                        }

                        div."field" {
                            label."hand" { "Options" }

                            div."options" {
                                div."option" {
                                    input."hand" name="option" required="true" placeholder="a choice" {}
                                    button."hand delete" type="button" { "delete" }
                                }
                                div."option" {
                                    input."hand" name="option" required="true" placeholder="a choice" {}
                                    button."hand delete" type="button" { "delete" }
                                }
                            }

                            button."hand" id="addOption" type="button" { "add option" }
                        }

                        button."hand submit" type="submit" { "create room" }
                    }
                }
                div {
                    img src="/static/img/vote.svg";
                }
            },
        )
    }

    async fn create_room(body: Vec<(String, String)>) -> Result<impl warp::Reply, warp::Rejection> {
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

        Ok(with_layout("Waiting", html!(), html!()))
    }

    pub fn routes() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        warp::get().map(homepage).or(warp::post()
            .and(warp::body::form::<Vec<(String, String)>>())
            .and_then(create_room))
    }
}
