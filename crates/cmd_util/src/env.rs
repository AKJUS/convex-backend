use std::{
    env,
    fmt::Debug,
    fs::File,
    str::FromStr,
    sync::LazyLock,
};

use tracing::Level;
use tracing_subscriber::{
    fmt::format::format,
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter,
    Layer,
};

pub fn env_config<T: Debug + FromStr>(name: &str, default: T) -> T
where
    <T as FromStr>::Err: Debug,
{
    let var_s = match env::var(name) {
        Ok(s) => s,
        Err(env::VarError::NotPresent) => return default,
        Err(env::VarError::NotUnicode(..)) => {
            tracing::warn!("Invalid value for {name}, falling back to {default:?}.");
            return default;
        },
    };
    match T::from_str(&var_s) {
        Ok(v) => {
            tracing::info!("Overriding {name} to {v:?} from environment");
            v
        },
        Err(e) => {
            tracing::warn!("Invalid value {var_s} for {name}, falling back to {default:?}: {e:?}");
            default
        },
    }
}

pub static CONVEX_TRACE_FILE: LazyLock<Option<File>> = LazyLock::new(|| {
    if env::var("CONVEX_TRACE_FILE").is_err() {
        return None;
    }

    let exe_path = std::env::current_exe().expect("Couldn't find exe name");
    let exe_name = exe_path
        .file_name()
        .expect("Path was empty")
        .to_str()
        .expect("Not valid unicode");
    // e.g. `backend.log`
    let filename = format!("{exe_name}.log");

    let file =
        File::create(&filename).unwrap_or_else(|_| panic!("Could not create file {filename}"));
    Some(file)
});

/// Guard object. Hold onto it for as long as you'd like to keep tracing to a
/// file specified by `CONVEX_TRACE_FILE`
pub struct TracingGuard {
    _guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

/// Call this from scripts and services at startup.
pub fn config_tool() -> TracingGuard {
    let mut layers = Vec::new();
    let color_disabled = std::env::var("NO_COLOR").is_ok();
    let format_layer = match std::env::var("LOG_FORMAT") {
        Ok(s) if s == "json" => tracing_subscriber::fmt::layer()
            .event_format(format().json())
            .with_ansi(!color_disabled)
            .boxed(),
        Ok(s) if s == "compact" => tracing_subscriber::fmt::layer()
            .event_format(format().compact())
            .with_ansi(!color_disabled)
            .boxed(),
        Ok(s) if s == "pretty" => tracing_subscriber::fmt::layer()
            .event_format(format().pretty())
            .with_ansi(!color_disabled)
            .boxed(),
        _ => tracing_subscriber::fmt::layer()
            .event_format(format().compact())
            .with_ansi(!color_disabled)
            .boxed(),
    };
    let stdout = format_layer
        .with_filter(tracing_subscriber::EnvFilter::from_default_env())
        .boxed();
    layers.push(stdout);

    let guard = if let Some(ref file) = *CONVEX_TRACE_FILE {
        let (file_writer, guard) = tracing_appender::non_blocking(file);
        let file_writer_layer = tracing_subscriber::fmt::layer()
            .with_writer(file_writer)
            .with_filter(
                EnvFilter::from_default_env()
                    .add_directive(Level::INFO.into())
                    .add_directive("common::errors=debug".parse().unwrap()),
            )
            .boxed();
        layers.push(file_writer_layer);
        Some(guard)
    } else {
        None
    };
    tracing_subscriber::registry().with(layers).init();

    TracingGuard { _guard: guard }
}

pub fn config_test() {
    // Try to initialize tracing_subcriber. Ok if it fails - probably
    // means it was initialized already. Ok to be non-rigorous here, because
    // it's very hard to run initialization of logging in tests, so we tend to
    // toss it in common helper methods all over.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .compact()
        .try_init();
}
