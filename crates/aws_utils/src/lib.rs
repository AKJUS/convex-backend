#![feature(coroutines)]
#![feature(exit_status_error)]
use std::{
    env,
    sync::LazyLock,
};

use aws_config::{
    environment::credentials::EnvironmentVariableCredentialsProvider,
    BehaviorVersion,
    ConfigLoader,
};
use aws_types::region::Region;

pub mod s3;

static S3_ENDPOINT_URL: LazyLock<Option<String>> =
    LazyLock::new(|| env::var("S3_ENDPOINT_URL").ok());

static AWS_ACCESS_KEY_ID: LazyLock<Option<String>> =
    LazyLock::new(|| env::var("AWS_ACCESS_KEY_ID").ok());

static AWS_SECRET_ACCESS_KEY: LazyLock<Option<String>> =
    LazyLock::new(|| env::var("AWS_SECRET_ACCESS_KEY").ok());

static AWS_REGION: LazyLock<Option<String>> = LazyLock::new(|| env::var("AWS_REGION").ok());

/// Similar aws_config::from_env but returns an error if credentials or
/// region is are not. It also doesn't spew out log lines every time
/// credentials are accessed.
pub fn must_config_from_env() -> anyhow::Result<ConfigLoader> {
    let Some(region) = AWS_REGION.clone() else {
        anyhow::bail!("AWS_REGION env variable must be set");
    };
    let region = Region::new(region);
    let Some(_) = AWS_ACCESS_KEY_ID.clone() else {
        anyhow::bail!("AWS_ACCESS_KEY_ID env variable must be set");
    };
    let Some(_) = AWS_SECRET_ACCESS_KEY.clone() else {
        anyhow::bail!("AWS_SECRET_ACCESS_KEY env variable must be set");
    };
    let credentials = EnvironmentVariableCredentialsProvider::new();
    Ok(aws_config::defaults(BehaviorVersion::v2025_01_17())
        .region(region)
        .credentials_provider(credentials))
}

pub fn must_s3_config_from_env() -> anyhow::Result<ConfigLoader> {
    let mut config_loader = must_config_from_env()?;
    if let Some(s3_endpoint_url) = S3_ENDPOINT_URL.clone() {
        config_loader = config_loader.endpoint_url(s3_endpoint_url);
    }
    Ok(config_loader)
}
