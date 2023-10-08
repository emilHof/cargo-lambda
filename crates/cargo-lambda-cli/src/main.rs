#![warn(rust_2018_idioms, unused_lifetimes, clippy::multiple_crate_versions)]
use cargo_lambda_build::{Build, Zig};
use cargo_lambda_deploy::Deploy;
use cargo_lambda_invoke::Invoke;
use cargo_lambda_new::{Init, New};
use cargo_lambda_watch::Watch;
use clap::{CommandFactory, Parser, Subcommand};
use clap_cargo::style::CLAP_STYLING;
use miette::{miette, IntoDiagnostic, Result};
use std::{boxed::Box, env, io::IsTerminal, path::PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo", disable_version_flag = true)]
#[command(styles = CLAP_STYLING)]
enum App {
    Lambda(Lambda),
    #[command(subcommand, hide = true)]
    Zig(Zig),
}

/// Cargo Lambda is a CLI to work with AWS Lambda functions locally
#[derive(Clone, Debug, Parser)]
struct Lambda {
    #[command(subcommand)]
    subcommand: Option<Box<LambdaSubcommand>>,
    /// Enable logs in any subcommand. Use `-v` for debug logs, and `-vv` for trace logs
    #[arg(short = 'v', long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
    /// Coloring: auto, always, never
    #[arg(long, default_value = "auto", value_name = "WHEN", global = true)]
    color: String,
    /// Print version information
    #[arg(short = 'V', long)]
    version: bool,
}

#[derive(Clone, Debug, Subcommand)]
enum LambdaSubcommand {
    /// `cargo lambda build` compiles AWS Lambda functions and extension natively.
    /// It produces artifacts which you can then upload to AWS Lambda with `cargo lambda deploy`,
    /// or use with other ecosystem tools, SAM Cli or the AWS CDK.
    Build(Box<Build>),
    /// `cargo lambda deploy` uploads functions and extensions to AWS Lambda.
    /// You can use the same command to create new functions as well as update existent functions code.
    Deploy(Box<Deploy>),
    /// `cargo lambda init` creates Rust Lambda packages in an existent directory.
    /// Files present in that directory will be preserved as they were before running this command.
    Init(Init),
    /// `cargo lambda invoke` sends requests to the control plane emulator to test and debug interactions with your Lambda functions.
    /// This command can also be used to send requests to remote functions once deployed on AWS Lambda.
    Invoke(Invoke),
    /// `cargo lambda new` creates Rust Lambda packages from a well defined template to help you start writing AWS Lambda functions in Rust.
    New(New),
    /// `cargo lambda watch` boots a development server that emulates interactions with the AWS Lambda control plane.
    /// This subcommand also reloads your Rust code as you work on it.
    Watch(Watch),
}

impl LambdaSubcommand {
    async fn run(self, color: &str) -> Result<()> {
        match self {
            Self::Build(mut b) => b.run().await,
            Self::Deploy(d) => d.run().await,
            Self::Init(mut i) => i.run().await,
            Self::Invoke(i) => i.run().await,
            Self::New(mut n) => n.run().await,
            Self::Watch(w) => w.run(color).await,
        }
    }
}

fn print_version() -> Result<()> {
    println!(
        "cargo-lambda {} {}",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_LAMBDA_BUILD_INFO")
    );
    Ok(())
}

fn print_help() -> Result<()> {
    let mut app = App::command();
    let lambda = app
        .find_subcommand_mut("lambda")
        .cloned()
        .map(|a| a.name("cargo lambda").bin_name("cargo lambda"));

    match lambda {
        Some(lambda) => lambda.styles(CLAP_STYLING).print_help().into_diagnostic(),
        None => {
            println!("Run `cargo lambda --help` to see usage");
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Zig might try to execute the same program again with "ar" as the name
    // to link specific static native libraries. We need to check
    // the program name before executing any operation to ensure
    // that the static linking works correctly.
    let mut args = env::args();
    let program_path = PathBuf::from(args.next().expect("missing program path"));
    let program_name = program_path.file_stem().expect("missing program name");

    miette::set_hook(Box::new(|_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .terminal_links(true)
                .footer("Was this error unexpected?\nOpen an issue in https://github.com/cargo-lambda/cargo-lambda/issues".into())
                .build(),
        )
    }))?;

    if program_name.eq_ignore_ascii_case("ar") {
        let zig = Zig::Ar {
            args: args.collect(),
        };
        zig.execute().map_err(|e| miette!(e))
    } else {
        run_subcommand().await
    }
}

async fn run_subcommand() -> Result<()> {
    let app = App::parse();

    let lambda = match app {
        App::Zig(zig) => return zig.execute().map_err(|e| miette!(e)),
        App::Lambda(lambda) => lambda,
    };

    if lambda.version {
        return print_version();
    }

    let subcommand = match lambda.subcommand {
        None => return print_help(),
        Some(subcommand) => subcommand,
    };

    let log_directive = if lambda.verbose == 0 {
        std::env::var("RUST_LOG").unwrap_or_else(|_| "cargo_lambda=info".into())
    } else if lambda.verbose == 1 {
        "cargo_lambda=debug".into()
    } else {
        "cargo_lambda=trace".into()
    };

    let fmt = tracing_subscriber::fmt::layer()
        .with_target(false)
        .without_time()
        .with_ansi(match lambda.color.as_str() {
            "auto" => std::io::stdout().is_terminal(),
            "always" => true,
            "never" => false,
            _ => {
                return Err(miette!(
                    "argument for --color must be auto, always, or never, but found {}",
                    lambda.color
                ))
            }
        });

    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(log_directive))
        .with(fmt);

    if let LambdaSubcommand::Watch(w) = &*subcommand {
        subscriber.with(w.xray_layer()).init();
    } else {
        subscriber.init();
    }

    subcommand.run(&lambda.color).await
}
