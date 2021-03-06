extern crate ansi_term;
#[macro_use]
extern crate failure;
extern crate home;
extern crate pbr;
extern crate reqwest;
extern crate structopt;
extern crate tar;
extern crate tee;
extern crate tempfile;
extern crate xz2;

use std::borrow::Cow;
use std::env::set_current_dir;
use std::fs::{create_dir_all, remove_dir_all, rename};
use std::io::{stderr, stdout, Write};
use std::iter::once;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::process::Command;
use std::time::Duration;

use ansi_term::Color::{Red, Yellow};
use failure::{err_msg, Error, Fail, ResultExt};
use pbr::{ProgressBar, Units};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH};
use reqwest::StatusCode;
use reqwest::{Client, ClientBuilder, Proxy};
use structopt::StructOpt;
use tar::Archive;
use tee::TeeReader;
use tempfile::{tempdir, tempdir_in};
use xz2::read::XzDecoder;

static SUPPORTED_CHANNELS: &[&str] = &["nightly", "beta", "stable"];

#[derive(StructOpt, Debug)]
struct Args {
    #[structopt(
        help = "full commit hashes of the rustc builds, all 40 digits are needed; \
                if omitted, the latest master commit will be installed"
    )]
    commits: Vec<String>,

    #[structopt(short = "n", long = "name", help = "the name to call the toolchain")]
    name: Option<String>,

    #[structopt(
        short = "a",
        long = "alt",
        help = "download the alt build instead of normal build"
    )]
    alt: bool,

    #[structopt(
        short = "s",
        long = "server",
        help = "the server path which stores the compilers",
        default_value = "https://s3-us-west-1.amazonaws.com/rust-lang-ci2"
    )]
    server: String,

    #[structopt(short = "i", long = "host", help = "the triples of host platform")]
    host: Option<String>,

    #[structopt(
        short = "t",
        long = "targets",
        help = "additional target platforms to install, besides the host platform"
    )]
    targets: Vec<String>,

    #[structopt(
        short = "c",
        long = "component",
        help = "additional components to install, besides rustc and rust-std"
    )]
    components: Vec<String>,

    #[structopt(
        long = "channel",
        help = "specify the channel of the commits instead of detecting it automatically"
    )]
    channel: Option<String>,

    #[structopt(
        short = "p",
        long = "proxy",
        help = "the HTTP proxy for all download requests"
    )]
    proxy: Option<String>,

    #[structopt(
        long = "github-token",
        help = "An authorization token to access GitHub APIs"
    )]
    github_token: Option<String>,

    #[structopt(
        long = "dry-run",
        help = "Only log the URLs, without downloading the artifacts"
    )]
    dry_run: bool,

    #[structopt(
        long = "force",
        short = "f",
        help = "Replace an existing toolchain of the same name"
    )]
    force: bool,

    #[structopt(
        long = "keep-going",
        short = "k",
        help = "Continue downloading toolchains even if some of them failed"
    )]
    keep_going: bool,
}

macro_rules! path_buf {
    ($($e:expr),*$(,)*) => { [$($e),*].iter().collect::<PathBuf>() }
}

fn download_tar_xz(
    client: Option<&Client>,
    url: &str,
    src: &Path,
    dest: &Path,
    commit: &str,
    component: &str,
    channel: &str,
    target: &str,
) -> Result<(), Error> {
    eprintln!("downloading <{}>...", url);

    if let Some(client) = client {
        let response = client.get(url).send()?;

        match response.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => bail!(
                "missing component `{}` on toolchain `{}` on channel `{}` for target `{}`",
                component,
                commit,
                channel,
                target,
            ),
            status => bail!("received status {} for GET {}", status, url),
        };

        let length = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.parse().ok())
            .unwrap_or(0);

        let err = stderr();
        let mut lock = err.lock();
        let mut progress_bar = ProgressBar::on(lock, length);
        progress_bar.set_units(Units::Bytes);
        progress_bar.set_max_refresh_rate(Some(Duration::from_secs(1)));

        {
            let response = TeeReader::new(response, &mut progress_bar);
            let response = XzDecoder::new(response);
            for entry in Archive::new(response).entries()? {
                let mut entry = entry?;
                let dest_path = match entry.path()?.strip_prefix(src) {
                    Ok(sub_path) => dest.join(sub_path),
                    Err(_) => continue,
                };
                create_dir_all(dest_path.parent().unwrap())?;
                entry.unpack(dest_path)?;
            }
        }

        progress_bar.finish_print("completed");
    }

    Ok(())
}

struct Toolchain<'a> {
    commit: &'a str,
    host_target: &'a str,
    rust_std_targets: &'a [&'a str],
    components: &'a [&'a str],
    dest: Cow<'a, String>,
}

fn install_single_toolchain(
    client: &Client,
    maybe_dry_client: Option<&Client>,
    prefix: &str,
    toolchains_path: &Path,
    toolchain: &Toolchain,
    override_channel: Option<&str>,
    force: bool,
) -> Result<(), Error> {
    let toolchain_path = toolchains_path.join(&*toolchain.dest);
    if toolchain_path.is_dir() {
        if force {
            if maybe_dry_client.is_some() {
                remove_dir_all(&toolchain_path)?;
            }
        } else {
            eprintln!("toolchain `{}` is already installed", toolchain.dest);
            return Ok(());
        }
    }

    let channel = if let Some(channel) = override_channel {
        channel
    } else {
        get_channel(client, prefix, &toolchain.commit)?
    };

    // download every component except rust-std.
    for component in once(&"rustc").chain(toolchain.components) {
        let component_filename = if *component == "rust-src" {
            // rust-src is the only target-independent component
            format!("{}-{}", component, channel)
        } else {
            format!("{}-{}-{}", component, channel, toolchain.host_target)
        };
        download_tar_xz(
            maybe_dry_client,
            &format!(
                "{}/{}/{}.tar.xz",
                prefix, toolchain.commit, &component_filename
            ),
            &path_buf![&component_filename, *component],
            Path::new(&*toolchain.dest),
            toolchain.commit,
            component,
            channel,
            toolchain.host_target,
        )?;
    }

    // download rust-std for every target.
    for target in toolchain.rust_std_targets {
        let rust_std_filename = format!("rust-std-{}-{}", channel, target);
        download_tar_xz(
            maybe_dry_client,
            &format!(
                "{}/{}/{}.tar.xz",
                prefix, toolchain.commit, rust_std_filename
            ),
            &path_buf![&rust_std_filename, &format!("rust-std-{}", target), "lib"],
            &path_buf![&toolchain.dest, "lib"],
            toolchain.commit,
            "rust-std",
            channel,
            target,
        )?;
    }

    // install.
    if maybe_dry_client.is_some() {
        rename(&*toolchain.dest, toolchain_path)?;
        eprintln!("toolchain `{}` is successfully installed!", toolchain.dest);
    } else {
        eprintln!(
            "toolchain `{}` will be installed to `{}` on real run",
            toolchain.dest,
            toolchain_path.display()
        );
    }

    Ok(())
}

fn fetch_master_commit(client: &Client, github_token: Option<&str>) -> Result<String, Error> {
    eprintln!("fetching master commit hash... ");
    let res = fetch_master_commit_via_git()
        .context("unable to fetch master commit via git, falling back to HTTP");
    if let Err(err) = res {
        report_warn(&err);
    }

    fetch_master_commit_via_http(client, github_token)
}

fn fetch_master_commit_via_git() -> Result<String, Error> {
    let mut output = Command::new("git")
        .args(&[
            "ls-remote",
            "https://github.com/rust-lang/rust.git",
            "master",
        ])
        .output()?;
    ensure!(output.status.success(), "git ls-remote exited with error");
    ensure!(
        output
            .stdout
            .get(..40)
            .map_or(false, |h| h.iter().all(|c| c.is_ascii_hexdigit())),
        "git ls-remote does not return a commit"
    );

    output.stdout.truncate(40);
    Ok(unsafe { String::from_utf8_unchecked(output.stdout) })
}

fn fetch_master_commit_via_http(
    client: &Client,
    github_token: Option<&str>,
) -> Result<String, Error> {
    let mut req = client.get("https://api.github.com/repos/rust-lang/rust/commits/master");
    req = req.header(ACCEPT, "application/vnd.github.VERSION.sha");
    if let Some(token) = github_token {
        req = req.header(AUTHORIZATION, format!("token {}", token));
    }
    let master_commit = req.send()?.error_for_status()?.text()?;
    if master_commit.len() == 40
        && master_commit
            .chars()
            .all(|c| '0' <= c && c <= '9' || 'a' <= c && c <= 'f')
    {
        let out = stdout();
        let mut lock = out.lock();
        lock.write_all(master_commit.as_bytes())?;
        lock.flush()?;
        eprintln!();
        Ok(master_commit)
    } else {
        bail!("unable to parse `{}` as a commit", master_commit)
    }
}

fn get_channel(client: &Client, prefix: &str, commit: &str) -> Result<&'static str, Error> {
    eprintln!("detecting the channel of the `{}` toolchain...", commit);

    for channel in SUPPORTED_CHANNELS {
        let url = format!("{}/{}/rust-src-{}.tar.xz", prefix, commit, channel);
        let resp = client.head(&url).send()?;

        match resp.status() {
            StatusCode::OK => return Ok(channel),
            StatusCode::NOT_FOUND | StatusCode::FORBIDDEN => {}
            status => bail!("unexpected status code {} for HEAD {}", status, url),
        }
    }

    bail!("toolchain `{}` doesn't exist in any channel", commit);
}

fn run() -> Result<(), Error> {
    let mut args = Args::from_args();

    let mut client_builder = ClientBuilder::new();
    if let Some(proxy) = args.proxy {
        client_builder = client_builder.proxy(Proxy::all(&proxy)?);
    }
    let client = client_builder.build()?;

    let rustup_home = home::rustup_home().expect("$RUSTUP_HOME is undefined?");
    let toolchains_path = rustup_home.join("toolchains");
    if !toolchains_path.is_dir() {
        bail!(
            "`{}` is not a directory. please reinstall rustup.",
            toolchains_path.display()
        );
    }

    if args.commits.len() > 1 && args.name.is_some() {
        return Err(err_msg(
            "name argument can only be provided with a single commit",
        ));
    }

    let host = args.host.as_ref().map(|s| &**s).unwrap_or(env!("HOST"));

    let components = args.components.iter().map(|s| &**s).collect::<Vec<_>>();

    let rust_std_targets = args
        .targets
        .iter()
        .map(|s| &**s)
        .chain(once(host))
        .collect::<Vec<_>>();

    let toolchains_dir = {
        let path = rustup_home.join("tmp");
        if path.is_dir() {
            tempdir_in(path)
        } else {
            tempdir()
        }
    }?;
    set_current_dir(toolchains_dir.path())?;

    let prefix = format!(
        "{}/rustc-builds{}",
        args.server,
        if args.alt { "-alt" } else { "" }
    );

    if args.commits.is_empty() {
        args.commits.push(fetch_master_commit(
            &client,
            args.github_token.as_ref().map(|s| &**s),
        )?);
    }

    let dry_run_client = if args.dry_run { None } else { Some(&client) };
    let mut failed = false;
    for commit in args.commits {
        let dest = if let Some(name) = args.name.as_ref() {
            Cow::Borrowed(name)
        } else if args.alt {
            Cow::Owned(format!("{}-alt", commit))
        } else {
            Cow::Borrowed(&commit)
        };

        let result = install_single_toolchain(
            &client,
            dry_run_client,
            &prefix,
            &toolchains_path,
            &Toolchain {
                commit: &commit,
                host_target: &host,
                rust_std_targets: &rust_std_targets,
                components: &components,
                dest,
            },
            args.channel.as_ref().map(|c| c.as_str()),
            args.force,
        );

        if args.keep_going {
            if let Err(err) = result {
                report_warn(
                    &err.context(format!("skipping toolchain `{}` due to a failure", commit)),
                );
                failed = true;
            }
        } else {
            result?;
        }
    }

    // Return the error only after downloading the toolchains that didn't fail
    if failed {
        Err(err_msg("failed to download some toolchains"))
    } else {
        Ok(())
    }
}

fn report_error(err: &Fail) {
    eprintln!("{} {}", Red.bold().paint("error:"), err);
    for cause in err.iter_causes() {
        eprintln!("{} {}", Red.bold().paint("caused by:"), cause);
    }
    exit(1);
}

fn report_warn(warn: &Fail) {
    eprintln!("{} {}", Yellow.bold().paint("warn:"), warn);
    for cause in warn.iter_causes() {
        eprintln!("{} {}", Yellow.bold().paint("caused by:"), cause);
    }
    eprintln!("");
}

fn main() {
    if let Err(err) = run() {
        report_error(err.as_fail());
    }
}
