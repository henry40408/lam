use axum::{
    body::Bytes, extract::State, http::StatusCode, response::IntoResponse, routing::post, Router,
};
use clap::{Parser, Subcommand};
use dashmap::DashMap;
use lam::{evaluate, EvalBuilder, LamStore};
use std::{
    fs,
    io::{self, Cursor, Read},
    path,
    sync::Arc,
};
use tower_http::trace::{self, TraceLayer};
use tracing::{error, info, Level};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(author,version,about,long_about=None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Evaluate a script file
    Eval {
        /// Script path
        #[arg(long)]
        file: Option<path::PathBuf>,
        /// Timeout
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Handle request with a script file
    Serve {
        /// Script path
        #[arg(long)]
        file: path::PathBuf,
        /// Timeout
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Bind
        #[arg(long, default_value = "127.0.0.1:3000")]
        bind: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Eval { file, timeout } => {
            let script = if let Some(f) = file {
                fs::read_to_string(f)?
            } else {
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .expect("either file or script via standard input should be provided");
                buf
            };
            let e = EvalBuilder::new(io::stdin(), script)
                .set_timeout(timeout)
                .build()?;
            let res = evaluate(&e)?;
            print!("{}", res.result);
        }
        Commands::Serve {
            bind,
            file,
            timeout,
        } => {
            serve_file(&file, &bind, timeout).await?;
        }
    }
    Ok(())
}

struct AppState {
    script: String,
    store: LamStore,
    timeout: u64,
}

async fn index_route(State(app_state): State<Arc<AppState>>, body: Bytes) -> impl IntoResponse {
    let e = match EvalBuilder::new(Cursor::new(body), app_state.script.clone())
        .set_timeout(app_state.timeout)
        .set_store(app_state.store.clone())
        .build()
    {
        Ok(e) => e,
        Err(err) => {
            error!("{:?}", err);
            return (StatusCode::BAD_REQUEST, "".to_string());
        }
    };
    let res = evaluate(&e);
    match res {
        Ok(res) => (StatusCode::OK, res.result),
        Err(err) => {
            error!("{:?}", err);
            (StatusCode::BAD_REQUEST, "".to_string())
        }
    }
}

async fn serve_file(file: &path::PathBuf, bind: &str, timeout: u64) -> anyhow::Result<()> {
    let env_filter = EnvFilter::builder()
        .with_default_directive(Level::INFO.into())
        .from_env_lossy();
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let script = fs::read_to_string(file)?;
    let store = Arc::new(DashMap::new());
    let app_state = Arc::new(AppState {
        script,
        store,
        timeout,
    });

    let app = Router::new()
        .route("/", post(index_route))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(app_state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("serving lua script on {bind}");
    axum::serve(listener, app).await?;

    Ok(())
}
