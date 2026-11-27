mod client;
mod config;
mod core;
mod daemon;
mod jev;
mod model;
mod paths;
mod proto;
mod routes;
mod setup;
mod source;
mod state;
mod surface;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "meridian-comms",
    version,
    about = "Hand work to other people's agents in a shared Slack channel, and answer in the same thread.\n\n\
A notice says a Slack message is waiting: run `read <message-ref>` to fetch it, then \
`send <message-ref> --message TEXT` to answer. A post wakes nobody unless you add `--to`."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a Slack message and its thread, or a channel's recent top-level messages.
    Read {
        /// Message ref, as in the notice: `<channel>/<id>`.
        message: Option<String>,
        /// Read a channel's recent top-level messages instead.
        #[arg(long, conflicts_with = "message")]
        channel: Option<String>,
        /// How many messages to print (the thread's first message is always kept).
        #[arg(long)]
        limit: Option<usize>,
        /// Print JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Post in a message's thread as the bot, or open a thread with --channel.
    ///
    /// A post wakes nobody unless you add --to. `--to @person` wakes whichever of
    /// that person's sessions the rule picks; `--to <ref>` (or a short ID from
    /// `list`) wakes that session even if it is idle. With --to and no --message,
    /// nothing is posted and the recipients are woken about that message: that is
    /// how a failed wake is retried. Exits non-zero when a wake failed.
    Send {
        /// Message ref to answer in. The thread it belongs to gets the post.
        message: Option<String>,
        /// Open a new thread in this channel instead.
        #[arg(long, conflicts_with = "message")]
        channel: Option<String>,
        /// Who to wake: `@person` or a ref (a short ID from `list` works too). Repeatable.
        #[arg(long, value_name = "WHO")]
        to: Vec<String>,
        /// The text to post. `-` reads it from stdin.
        #[arg(long = "message", value_name = "TEXT")]
        message_text: Option<String>,
        /// Print JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Print the people, the sessions that could take a message, and waiting messages.
    List {
        /// A project to list instead of the caller's own.
        #[arg(long)]
        project: Option<String>,
        /// Print JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Hand a message waiting in an inbox to the daemon's router.
    ///
    /// Run as the hook for a new inbox message (for Meridian: an
    /// `inbox.received` row whose command is `meridian-comms route`), it takes
    /// the item from the hook's environment. It returns as soon as the daemon
    /// has accepted the item; `routes` shows what the router then did. An item
    /// routed once is not routed again.
    Route {
        /// The inbox item's ID, when not run as a hook.
        item: Option<String>,
    },
    /// Print the routing log: per routed item, how the routing ended.
    Routes {
        /// Only the routings of this inbox item ID or thread message ref.
        of: Option<String>,
        /// Print JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    #[command(hide = true)]
    Daemon,
    /// Print the Slack app manifest, or with `router` set up the router's second step.
    Setup {
        /// The owner's name for the manifest, when there is no config yet.
        #[arg(long)]
        owner: Option<String>,
        #[command(subcommand)]
        what: Option<Setup>,
    },
}

#[derive(Subcommand)]
enum Setup {
    /// Create the router directory and check Meridian can start the `inbox-router` agent there.
    ///
    /// The router's second step starts that agent, through Meridian, for a
    /// message its model can't place from the session list alone. Meridian
    /// ships the agent; its model, harness and effort go in `[inbox.router]`
    /// in `~/.meridian/config.toml`. The directory is a Meridian project of
    /// its own under the state directory. Run this once on the daemon's
    /// machine, outside any agent session, and again if an earlier version of
    /// meridian-comms set the router up. Without it the second step
    /// fails, the routing log says so, and the message waits.
    Router,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if matches!(cli.command, Command::Daemon) {
        tracing_subscriber::fmt()
            .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
            .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "meridian_comms=info".into()))
            .init();
    }
    let result = match cli.command {
        Command::Read {
            message,
            channel,
            limit,
            json,
        } => client::read(message, channel, limit, json).await,
        Command::Send {
            message,
            channel,
            to,
            message_text,
            json,
        } => client::send(message, channel, to, message_text, json).await,
        Command::List { project, json } => client::list(project, json).await,
        Command::Route { item } => client::route(item).await,
        Command::Routes { of, json } => client::routes(of, json),
        Command::Daemon => daemon::run().await,
        Command::Setup { what: None, owner } => setup::run(owner),
        Command::Setup {
            what: Some(Setup::Router), ..
        } => source::meridian::router::setup().await,
    };
    if let Err(e) = result {
        eprintln!("meridian-comms: {e:#}");
        std::process::exit(2);
    }
}
