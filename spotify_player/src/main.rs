mod auth;
mod cli;
mod client;
mod command;
mod config;
mod event;
mod key;
#[cfg(feature = "media-control")]
mod media_control;
mod state;
#[cfg(feature = "streaming")]
mod streaming;
mod token;
mod ui;
mod utils;

use anyhow::{Context, Result};
use std::io::Write;

fn init_app_cli_arguments() -> clap::ArgMatches {
    let cmd = clap::Command::new("spotify_player")
        .version("0.13.1")
        .about("A command driven spotify player")
        .author("Thang Pham <phamducthang1234@gmail>")
        .subcommand(cli::init_get_subcommand())
        .subcommand(cli::init_playback_subcommand())
        .subcommand(cli::init_connect_subcommand())
        .arg(
            clap::Arg::new("theme")
                .short('t')
                .long("theme")
                .value_name("THEME")
                .help("Application theme (default: dracula)")
        )
        .arg(
            clap::Arg::new("config-folder")
                .short('c')
                .long("config-folder")
                .value_name("FOLDER")
                .help("Path to the application's config folder (default: $HOME/.config/spotify-player)")
                .next_line_help(true)
        )
        .arg(
            clap::Arg::new("cache-folder")
                .short('C')
                .long("cache-folder")
                .value_name("FOLDER")
                .help("Path to the application's cache folder (default: $HOME/.cache/spotify-player)")
                .next_line_help(true)
        );

    #[cfg(feature = "daemon")]
    let cmd = cmd.arg(
        clap::Arg::new("daemon")
            .short('d')
            .long("daemon")
            .action(clap::ArgAction::SetTrue)
            .help("Running the application as a daemon"),
    );

    cmd.get_matches()
}

async fn init_spotify(
    client_pub: &flume::Sender<event::ClientRequest>,
    streaming_sub: &flume::Receiver<()>,
    client: &client::Client,
    state: &state::SharedState,
) -> Result<()> {
    // if `streaming` feature is enabled, create a new streaming connection
    #[cfg(feature = "streaming")]
    if state.app_config.enable_streaming {
        client
            .new_streaming_connection(streaming_sub.clone(), client_pub.clone())
            .context("failed to create a new streaming connection")?;
    }

    // initialize the playback state
    client.update_current_playback_state(state).await?;

    if state.player.read().playback.is_none() {
        tracing::info!("No playback found on startup, trying to connect to an available device...");
        client_pub.send(event::ClientRequest::ConnectDevice(None))?;
    }

    // request user data
    client_pub.send(event::ClientRequest::GetCurrentUserQueue)?;
    client_pub.send(event::ClientRequest::GetCurrentUser)?;
    client_pub.send(event::ClientRequest::GetUserPlaylists)?;
    client_pub.send(event::ClientRequest::GetUserFollowedArtists)?;
    client_pub.send(event::ClientRequest::GetUserSavedAlbums)?;
    client_pub.send(event::ClientRequest::GetUserSavedTracks)?;

    Ok(())
}

fn init_logging(cache_folder: &std::path::Path) -> Result<()> {
    let log_prefix = format!(
        "spotify-player-{}",
        chrono::Local::now().format("%y-%m-%d-%H-%M")
    );

    // initialize the application's logging
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "spotify_player=info"); // default to log the current crate only
    }
    let log_file = std::fs::File::create(cache_folder.join(format!("{log_prefix}.log")))
        .context("failed to create log file")?;
    tracing_subscriber::fmt::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(log_file))
        .init();

    // initialize the application's panic backtrace
    let backtrace_file =
        std::fs::File::create(cache_folder.join(format!("{log_prefix}.backtrace")))
            .context("failed to create backtrace file")?;
    let backtrace_file = std::sync::Mutex::new(backtrace_file);
    std::panic::set_hook(Box::new(move |info| {
        let mut file = backtrace_file.lock().unwrap();
        let backtrace = backtrace::Backtrace::new();
        writeln!(&mut file, "Got a panic: {info:#?}\n").unwrap();
        writeln!(&mut file, "Stack backtrace:\n{backtrace:?}").unwrap();
    }));

    Ok(())
}

#[tokio::main]
async fn start_app(state: state::SharedState, is_daemon: bool) -> Result<()> {
    // client channels
    let (client_pub, client_sub) = flume::unbounded::<event::ClientRequest>();
    // streaming channels, which are used to notify a shutdown to running streaming connections
    // upon creating a new connection.
    let (streaming_pub, streaming_sub) = flume::unbounded::<()>();

    // create a librespot session
    let session = auth::new_session(
        &state.cache_folder,
        state.app_config.device.audio_cache,
        &state.app_config,
    )
    .await?;

    // create a spotify API client
    let client = client::Client::new(
        session.clone(),
        state.app_config.device.clone(),
        state.app_config.client_id.clone(),
    );
    client.init_token().await?;

    // initialize Spotify-related stuff
    if is_daemon {
        #[cfg(feature = "streaming")]
        client
            .new_streaming_connection(streaming_sub.clone(), client_pub.clone())
            .context("failed to create a new streaming connection")?;
    } else {
        init_spotify(&client_pub, &streaming_sub, &client, &state)
            .await
            .context("failed to initialize the spotify client")?;
    }

    // Spawn application's tasks
    let mut tasks = Vec::new();

    // client socket task (for handling CLI commands)
    tasks.push(tokio::task::spawn({
        let client = client.clone();
        let state = state.clone();
        async move {
            if let Err(err) = cli::start_socket(client, state).await {
                tracing::warn!("Failed to run client socket for CLI: {err}");
            }
        }
    }));

    // client event handler task
    tasks.push(tokio::task::spawn({
        let state = state.clone();
        let client_pub = client_pub.clone();
        async move {
            client::start_client_handler(
                state,
                client,
                client_pub,
                client_sub,
                streaming_pub,
                streaming_sub,
            )
            .await;
        }
    }));

    // player event watcher task
    tasks.push(tokio::task::spawn({
        let state = state.clone();
        let client_pub = client_pub.clone();
        async move {
            client::start_player_event_watchers(state, client_pub).await;
        }
    }));

    if !is_daemon {
        // spawn tasks needed for running the application UI

        // terminal event handler task
        tokio::task::spawn_blocking({
            let client_pub = client_pub.clone();
            let state = state.clone();
            move || {
                event::start_event_handler(state, client_pub);
            }
        });

        // application UI task
        tokio::task::spawn_blocking({
            let state = state.clone();
            move || ui::run(state)
        });
    }

    #[cfg(feature = "media-control")]
    if state.app_config.enable_media_control {
        // media control task
        tokio::task::spawn_blocking({
            let state = state.clone();
            move || {
                if let Err(err) = media_control::start_event_watcher(state, client_pub) {
                    tracing::error!(
                        "Failed to start the application's media control event watcher: err={err:?}"
                    );
                }
            }
        });

        // the winit's event loop must be run in the main thread
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            // Start an event loop that listens to OS window events.
            //
            // MacOS and Windows require an open window to be able to listen to media
            // control events. The below code will create an invisible window on startup
            // to listen to such events.
            let event_loop = winit::event_loop::EventLoop::new();
            event_loop.run(move |_, _, control_flow| {
                *control_flow = winit::event_loop::ControlFlow::Wait;
            });
        }
    }

    for task in tasks {
        task.await?;
    }

    Ok(())
}

fn main() -> Result<()> {
    // parse command line arguments
    let args = init_app_cli_arguments();

    // initialize the application's cache folder and config folder
    let config_folder = match args.get_one::<String>("config-folder") {
        Some(path) => path.into(),
        None => config::get_config_folder_path()?,
    };
    if !config_folder.exists() {
        std::fs::create_dir_all(&config_folder)?;
    }

    let cache_folder = match args.get_one::<String>("cache-folder") {
        Some(path) => path.into(),
        None => config::get_cache_folder_path()?,
    };
    let cache_audio_folder = cache_folder.join("audio");
    if !cache_audio_folder.exists() {
        std::fs::create_dir_all(&cache_audio_folder)?;
    }
    let cache_image_folder = cache_folder.join("image");
    if !cache_image_folder.exists() {
        std::fs::create_dir_all(&cache_image_folder)?;
    }

    // initialize the application's log
    init_logging(&cache_folder).context("failed to initialize application's logging")?;

    // initialize the application state
    let state = {
        let mut state = state::State {
            cache_folder,
            ..state::State::default()
        };
        // parse config options from the config files into application's state
        state.parse_config_files(&config_folder, args.get_one::<String>("theme"))?;
        std::sync::Arc::new(state)
    };

    match args.subcommand() {
        None => {
            #[cfg(feature = "daemon")]
            {
                let is_daemon = args.get_flag("daemon");
                if is_daemon {
                    if cfg!(any(target_os = "macos", target_os = "windows"))
                        && cfg!(feature = "media-control")
                    {
                        eprintln!("Running the application as a daemon on windows/macos with `media-control` feature enabled is not supported!");
                        std::process::exit(1);
                    }
                    if cfg!(not(feature = "streaming")) {
                        eprintln!("`streaming` feature must be enabled to run the application as a daemon!");
                        std::process::exit(1);
                    }

                    tracing::info!("Starting the application as a daemon...");
                    let daemonize = daemonize::Daemonize::new();
                    daemonize.start()?;
                }
                start_app(state, is_daemon)
            }

            #[cfg(not(feature = "daemon"))]
            start_app(state, false)
        }
        Some((cmd, args)) => cli::handle_cli_subcommand(cmd, args, state.app_config.client_port),
    }
}
