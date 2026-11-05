mod ui;

use std::{
    collections::HashMap,
    fs,
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use mate::{
    broker::BrokerStore,
    certificates::{CertificateKind, CertificateService},
    clipboard,
    config::{AppConfig, BrokerConfig, SubscriptionConfig, config_path},
    mqtt::{BrokerEvent, BrokerRuntime, spawn_broker, test_connection},
    mqttlog::MqttLogger,
};
use ui::{UiAction, UiState};

const CONNECTION_TEST_TIMEOUT: Duration = Duration::from_secs(10);
const PROTOCOLS: [&str; 2] = ["mqtt311", "mqtt5"];
const EVENT_QUEUE_CAPACITY: usize = 1024;
const MAX_EVENTS_PER_ITERATION: usize = 2000;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(30);
/// Shorter while a replay runs, so messages leave close to their due time.
const REPLAY_POLL_INTERVAL: Duration = Duration::from_millis(4);

#[derive(Debug, Parser)]
#[command(
    name = "mate",
    version,
    about = "MQTT Administration & Terminal Environment"
)]
struct Args {
    #[arg(
        long,
        value_name = "NAME",
        help = "Select a configured broker profile; repeat it with --connect to connect several"
    )]
    broker: Vec<String>,
    #[arg(
        long,
        value_name = "PATH",
        help = "Use an explicit TOML configuration file"
    )]
    config: Option<PathBuf>,
    #[arg(
        long,
        help = "Connect to the selected broker(s) (--broker, else the default or only one) right away"
    )]
    connect: bool,
    #[arg(long, help = "Enable debug logging to the standard error stream")]
    debug: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Broker {
        #[command(subcommand)]
        command: BrokerCommand,
    },
}

#[derive(Debug, Subcommand)]
enum BrokerCommand {
    List,
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 1883)]
        port: u16,
        /// MQTT version of the connection.
        #[arg(long, default_value = "mqtt311", value_parser = PROTOCOLS)]
        protocol: String,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password_ref: Option<String>,
        #[arg(long)]
        tls: bool,
        #[arg(long)]
        ca: Option<PathBuf>,
        #[arg(long)]
        client_cert: Option<PathBuf>,
        #[arg(long)]
        client_key: Option<PathBuf>,
        #[arg(long)]
        subscribe: Option<String>,
        #[arg(long, default_value_t = 0)]
        qos: u8,
        #[arg(long)]
        default: bool,
        /// Connect to this broker when MATE starts with it selected (--broker) or as the default.
        #[arg(long)]
        auto_connect: bool,
    },
    Edit {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        password_ref: Option<String>,
        /// MQTT client ID; some brokers only accept certain IDs. Setting it turns the random suffix off.
        #[arg(long)]
        client_id: Option<String>,
        /// Append a random suffix to the client ID per connection so several instances can run (true/false).
        #[arg(long)]
        unique_client_id: Option<bool>,
        /// Enable or disable TLS (true/false).
        #[arg(long)]
        tls: Option<bool>,
        /// Allow or forbid publishing from MATE for this broker (true/false).
        #[arg(long)]
        allow_publish: Option<bool>,
        /// Also subscribe to the broker's $SYS/# system topics (true/false).
        #[arg(long)]
        system_topics: Option<bool>,
        /// Connect to this broker when MATE starts with it selected or as the default (true/false).
        #[arg(long)]
        auto_connect: Option<bool>,
        /// MQTT version of the connection.
        #[arg(long, value_parser = PROTOCOLS)]
        protocol: Option<String>,
    },
    Remove {
        id: String,
        #[arg(long, required = true)]
        yes: bool,
    },
    Test {
        id: String,
    },
    /// Copy a profile; certificates imported into the original are not copied.
    Duplicate {
        id: String,
    },
    /// List the subscriptions of a profile.
    Subscriptions {
        id: String,
    },
    /// Add a subscription (or change the QoS of an existing one).
    Subscribe {
        id: String,
        topic: String,
        #[arg(long, default_value_t = 0)]
        qos: u8,
    },
    /// Remove a subscription.
    Unsubscribe {
        id: String,
        topic: String,
    },
    /// Drop messages matching a topic filter (for example '+/hmi/#') whatever the subscriptions deliver.
    Exclude {
        id: String,
        filter: String,
    },
    /// Remove an exclusion again.
    Unexclude {
        id: String,
        filter: String,
    },
    /// Enable a subscription again.
    SubscriptionEnable {
        id: String,
        topic: String,
    },
    /// Keep a subscription in the profile but stop subscribing to it.
    SubscriptionDisable {
        id: String,
        topic: String,
    },
    /// Set the profile used when none is selected, or clear it with --clear.
    Default {
        #[arg(required_unless_present = "clear")]
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        clear: bool,
    },
    /// Prompt for the broker password and store it in the OS keyring.
    Password {
        id: String,
    },
    Certs {
        id: String,
    },
    CertImport {
        id: String,
        kind: String,
        source: PathBuf,
        #[arg(long)]
        replace: bool,
    },
    CertReference {
        id: String,
        kind: String,
        path: PathBuf,
    },
    CertRemove {
        id: String,
        kind: String,
    },
    Export {
        id: String,
        path: PathBuf,
    },
    ExportCertificates {
        id: String,
        destination: PathBuf,
        #[arg(long)]
        include_private_key: bool,
        #[arg(long, required = true)]
        confirm_sensitive: bool,
    },
    Import {
        path: PathBuf,
    },
    ImportCertificates {
        directory: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging(args.debug);

    let config_file = match args.config {
        Some(path) => path,
        None => config_path()?,
    };
    let broker_store = BrokerStore::from_config_path(&config_file)?;
    let config = broker_store.load()?;
    if let Some(command) = args.command {
        run_command(command, broker_store).await?;
        return Ok(());
    }
    for name in args.broker.iter().map(String::as_str) {
        anyhow::ensure!(
            config
                .brokers
                .iter()
                .any(|broker| broker.name == name || broker.id == name),
            "no broker profile named '{name}' is configured"
        );
    }

    let terminal = ratatui::init();
    // Pasted text arrives as one event instead of key presses that would trigger shortcuts.
    let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste);
    // Terminals that keep a title stack get their old title back when MATE ends; others ignore this.
    let _ = crossterm::execute!(io::stdout(), crossterm::style::Print("\x1b[22;0t"));
    let result = run(terminal, config, broker_store, args.broker, args.connect).await;
    let _ = crossterm::execute!(io::stdout(), crossterm::style::Print("\x1b[23;0t"));
    let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn run_command(command: Command, store: BrokerStore) -> Result<()> {
    match command {
        Command::Broker { command } => run_broker_command(command, store).await,
    }
}

async fn run_broker_command(command: BrokerCommand, store: BrokerStore) -> Result<()> {
    match command {
        BrokerCommand::List => {
            for profile in store.list_profiles()? {
                println!(
                    "{}\t{}\t{}:{}\t{}",
                    profile.id, profile.name, profile.host, profile.port, profile.protocol
                );
            }
        }
        BrokerCommand::Add {
            name,
            host,
            port,
            protocol,
            username,
            password_ref,
            tls,
            ca,
            client_cert,
            client_key,
            subscribe,
            qos,
            default,
            auto_connect,
        } => {
            anyhow::ensure!(
                username.is_some() == password_ref.is_some(),
                "--username and --password-ref must be used together"
            );
            anyhow::ensure!(qos <= 2, "--qos must be 0, 1, or 2");
            let mut profile = store.create(BrokerConfig {
                name,
                host,
                port,
                protocol,
                auth: mate::config::AuthConfig {
                    username,
                    password_ref,
                },
                tls: mate::config::TlsConfig {
                    enabled: tls,
                    ..mate::config::TlsConfig::default()
                },
                subscriptions: subscribe
                    .into_iter()
                    .map(|topic| SubscriptionConfig::new(topic, qos))
                    .collect(),
                auto_connect,
                ..BrokerConfig::default()
            })?;
            let service = CertificateService::new(store.clone());
            for (kind, path) in [
                (CertificateKind::Ca, ca),
                (CertificateKind::ClientCertificate, client_cert),
                (CertificateKind::ClientPrivateKey, client_key),
            ] {
                let Some(path) = path else { continue };
                // Relative CLI paths mean "relative to the shell", not to the broker directory.
                let result = std::path::absolute(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|absolute| service.set_reference(&profile.id, kind, &absolute));
                if let Err(error) = result {
                    let _ = store.remove(&profile.id);
                    return Err(error.context(format!("invalid {}", kind.filename())));
                }
            }
            println!("created broker {} ({})", profile.name, profile.id);
            if default {
                let mut app = store.load()?;
                app.general.default_broker = Some(profile.id.clone());
                store.save_general(&app)?;
            }
            profile.broker_dir = None;
        }
        BrokerCommand::Edit {
            id,
            name,
            host,
            port,
            username,
            password_ref,
            client_id,
            unique_client_id,
            tls,
            allow_publish,
            system_topics,
            auto_connect,
            protocol,
        } => {
            anyhow::ensure!(
                username.is_some() == password_ref.is_some()
                    || username.is_none() && password_ref.is_none(),
                "--username and --password-ref must be used together"
            );
            anyhow::ensure!(
                client_id.as_deref().is_none_or(|id| !id.trim().is_empty()),
                "--client-id must not be empty"
            );
            let renamed = name.is_some();
            let mut profile = store.update(&id, |profile| {
                if let Some(client_id) = client_id {
                    profile.client_id = client_id;
                    profile.unique_client_id = false;
                }
                if let Some(unique) = unique_client_id {
                    profile.unique_client_id = unique;
                }
                if let Some(tls) = tls {
                    profile.tls.enabled = tls;
                }
                if let Some(allow_publish) = allow_publish {
                    profile.allow_publish = allow_publish;
                }
                if let Some(system_topics) = system_topics {
                    profile.show_system_topics = system_topics;
                }
                if let Some(auto_connect) = auto_connect {
                    profile.auto_connect = auto_connect;
                }
                if let Some(protocol) = protocol {
                    profile.protocol = protocol;
                }
                if let Some(name) = name {
                    profile.name = name;
                }
                if let Some(host) = host {
                    profile.host = host;
                }
                if let Some(port) = port {
                    profile.port = port;
                }
                if let Some(username) = username {
                    profile.auth.username = Some(username);
                }
                if let Some(password_ref) = password_ref {
                    profile.auth.password_ref = Some(password_ref);
                }
            })?;
            if renamed {
                profile = store.follow_name(&id)?;
            }
            println!("updated broker {} ({})", profile.name, profile.id);
            if profile.id != id {
                println!("its folder and ID changed from {id} to {}", profile.id);
            }
        }
        BrokerCommand::Remove { id, yes } => {
            anyhow::ensure!(yes, "broker removal requires --yes");
            let mut app = store.load()?;
            if app.general.default_broker.as_deref() == Some(&id) {
                app.general.default_broker = None;
                store.save_general(&app)?;
            }
            store.remove(&id)?;
            println!("removed broker {id}");
        }
        BrokerCommand::Test { id } => test_broker(&store, &id).await?,
        BrokerCommand::Subscriptions { id } => {
            for subscription in store.get(&id)?.subscriptions {
                println!(
                    "{}\t{}\t{}",
                    subscription.topic,
                    if subscription.exclude {
                        "exclude".to_owned()
                    } else {
                        format!("qos {}", subscription.qos)
                    },
                    if subscription.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
            }
        }
        BrokerCommand::Subscribe { id, topic, qos } => {
            store.add_subscription(&id, &topic, qos)?;
            println!("{id} subscribes to {} (QoS {qos})", topic.trim());
        }
        BrokerCommand::Unsubscribe { id, topic } => {
            store.remove_subscription(&id, &topic)?;
            println!("removed subscription {topic} from {id}");
        }
        BrokerCommand::Exclude { id, filter } => {
            store.add_exclusion(&id, &filter)?;
            println!("{id} drops messages matching {}", filter.trim());
        }
        BrokerCommand::Unexclude { id, filter } => {
            ensure!(
                store
                    .get(&id)?
                    .subscriptions
                    .iter()
                    .any(|entry| entry.exclude && entry.topic == filter.trim()),
                "broker '{id}' has no exclusion '{}'",
                filter.trim()
            );
            store.remove_subscription(&id, filter.trim())?;
            println!("removed exclusion {} from {id}", filter.trim());
        }
        BrokerCommand::SubscriptionEnable { id, topic } => {
            store.set_subscription_enabled(&id, &topic, true)?;
            println!("enabled subscription {topic} of {id}");
        }
        BrokerCommand::SubscriptionDisable { id, topic } => {
            store.set_subscription_enabled(&id, &topic, false)?;
            println!("disabled subscription {topic} of {id}");
        }
        BrokerCommand::Duplicate { id } => {
            let original = store.get(&id)?;
            let copy = store.duplicate_profile(&id)?;
            println!(
                "duplicated {} as {} ({})",
                original.name, copy.name, copy.id
            );
            let had_imported = [
                &original.tls.ca_file,
                &original.tls.client_cert,
                &original.tls.client_key,
            ]
            .into_iter()
            .flatten()
            .any(|path| path.is_relative());
            if had_imported {
                println!(
                    "note: certificates imported into {id} were not copied; import or reference them for {}",
                    copy.id
                );
            }
        }
        BrokerCommand::Default { id, clear } => {
            store.set_default_broker(if clear { None } else { id.as_deref() })?;
            match id.filter(|_| !clear) {
                Some(id) => println!("default broker is now {id}"),
                None => println!("default broker cleared"),
            }
        }
        BrokerCommand::Password { id } => {
            let profile = store.get(&id)?;
            let username = profile.auth.username.as_deref().unwrap_or("(none)");
            let prompt = format!("Password for {username}@{}: ", profile.host);
            let password = rpassword::prompt_password(prompt)?;
            mate::security::store_broker_password(&profile, &password)?;
            println!("password stored in the operating-system credential store");
        }
        BrokerCommand::Certs { id } => {
            let service = CertificateService::new(store);
            for cert in service.list(&id)? {
                println!(
                    "{}\t{}\t{}",
                    cert.kind.filename(),
                    cert.path.display(),
                    if cert.imported {
                        "imported"
                    } else {
                        "external"
                    }
                );
                if let Some(subject) = cert.subject {
                    println!("  subject: {subject}");
                }
                if let Some(issuer) = cert.issuer {
                    println!("  issuer: {issuer}");
                }
                if let Some(not_after) = cert.not_after {
                    println!(
                        "  expires: {not_after}{}",
                        if cert.expired { " (expired)" } else { "" }
                    );
                }
                if let Some(error) = cert.error {
                    println!("  error: {error}");
                }
            }
        }
        BrokerCommand::CertImport {
            id,
            kind,
            source,
            replace,
        } => {
            let kind = CertificateKind::parse(&kind)?;
            let path = CertificateService::new(store).import(&id, kind, &source, replace)?;
            println!("imported {} to {}", kind.filename(), path.display());
        }
        BrokerCommand::CertReference { id, kind, path } => {
            let kind = CertificateKind::parse(&kind)?;
            CertificateService::new(store).set_reference(&id, kind, &path)?;
            println!("configured external reference for {}", kind.filename());
        }
        BrokerCommand::CertRemove { id, kind } => {
            let kind = CertificateKind::parse(&kind)?;
            CertificateService::new(store).remove(&id, kind)?;
            println!("removed {} configuration", kind.filename());
        }
        BrokerCommand::Export { id, path } => {
            let profile = store.export_profile(&id)?;
            let encoded = toml::to_string_pretty(&profile)?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| {
                    anyhow::anyhow!("could not create export {}: {error}", path.display())
                })?;
            std::io::Write::write_all(&mut file, encoded.as_bytes())?;
            println!("exported broker profile to {}", path.display());
        }
        BrokerCommand::ExportCertificates {
            id,
            destination,
            include_private_key,
            confirm_sensitive,
        } => {
            CertificateService::new(store).export_bundle(
                &id,
                &destination,
                include_private_key,
                confirm_sensitive,
            )?;
            println!("exported certificate bundle to {}", destination.display());
        }
        BrokerCommand::Import { path } => {
            let source = fs::read_to_string(&path).map_err(|error| {
                anyhow::anyhow!("could not read profile {}: {error}", path.display())
            })?;
            let profile: BrokerConfig = toml::from_str(&source)
                .map_err(|error| anyhow::anyhow!("invalid broker profile TOML: {error}"))?;
            let profile = store.import_profile(profile)?;
            println!("imported broker {} as {}", profile.name, profile.id);
        }
        BrokerCommand::ImportCertificates { directory } => {
            let profile = CertificateService::new(store).import_bundle(&directory)?;
            println!(
                "imported certificate bundle as broker {} ({})",
                profile.name, profile.id
            );
        }
    }
    Ok(())
}

async fn check_profile(store: BrokerStore, profile: BrokerConfig) -> Result<()> {
    CertificateService::new(store).validate_profile(&profile)?;
    test_connection(profile, CONNECTION_TEST_TIMEOUT).await
}

async fn test_broker(store: &BrokerStore, id: &str) -> Result<()> {
    let profile = store.get(id)?;
    let endpoint = format!("{}:{}", profile.host, profile.port);
    check_profile(store.clone(), profile).await?;
    println!("connected successfully to {endpoint}");
    Ok(())
}

fn init_logging(debug: bool) {
    let default_filter = if debug { "debug" } else { "warn" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
}

/// Copies through a system clipboard tool, otherwise through OSC 52; returns the notice to show.
fn copy_to_clipboard(request: &ui::ClipboardRequest) -> Result<(String, bool)> {
    let size = request.text.len();
    let what = &request.what;
    let native_error = match clipboard::copy_native(&request.text) {
        Ok(tool) => return Ok((format!("Copied {what} ({size} bytes) with {tool}"), false)),
        Err(error) => error,
    };
    match clipboard::osc52_sequence(&request.text) {
        Ok(sequence) => {
            let mut stdout = io::stdout();
            stdout.write_all(sequence.as_bytes())?;
            stdout.flush()?;
            Ok((
                format!(
                    "Copied {what} ({size} bytes) via OSC 52 (no clipboard tool: {native_error}); the terminal must allow it"
                ),
                false,
            ))
        }
        Err(error) => Ok((format!("Cannot copy {what}: {error}; {native_error}"), true)),
    }
}

/// A broker connection of this run; `generation` tells its events from those of an earlier one.
struct Running {
    runtime: BrokerRuntime,
    generation: u64,
    dropped: Arc<AtomicU64>,
}

type Runtimes = HashMap<String, Running>;
type TaggedEvent = (String, u64, BrokerEvent);

/// Writes a message to the broker's log file when logging is on, then hands the event to the UI.
fn deliver(state: &mut UiState, logger: &mut MqttLogger, broker_id: &str, event: BrokerEvent) {
    if let BrokerEvent::Message(message) = &event
        && let Some(directory) = state.log_directory()
        && let Err(error) = logger.log(
            &directory,
            state.log_max_bytes(),
            broker_id,
            message.wall,
            &message.topic,
            &message.payload,
            message.properties.as_deref(),
        )
    {
        logger.close();
        state.logging_failed(&format!("{error:#}"));
    }
    state.apply_event_from(broker_id, event);
}

async fn stop_runtimes(
    runtimes: &mut Runtimes,
    event_rx: &mut mpsc::Receiver<TaggedEvent>,
    state: &mut UiState,
    logger: &mut MqttLogger,
) {
    if runtimes.is_empty() {
        return;
    }
    for (_, running) in runtimes.drain() {
        running.runtime.stop().await;
    }
    // Keep messages already queued, but drop stale state events of the stopped connections.
    while let Ok((id, _, event)) = event_rx.try_recv() {
        if matches!(event, BrokerEvent::Message(_)) {
            deliver(state, logger, &id, event);
        }
    }
}

/// Replaces the running connections (if any) by one to each of `profiles`.
async fn start_connections(
    profiles: Vec<BrokerConfig>,
    state: &mut UiState,
    runtimes: &mut Runtimes,
    generation: &mut u64,
    event_tx: &mpsc::Sender<TaggedEvent>,
    event_rx: &mut mpsc::Receiver<TaggedEvent>,
    logger: &mut MqttLogger,
) {
    stop_runtimes(runtimes, event_rx, state, logger).await;
    state.begin_connections(&profiles);
    for profile in profiles {
        *generation += 1;
        let id = profile.id.clone();
        let (tx, mut rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        match spawn_broker(profile, tx) {
            Ok((runtime, dropped)) => {
                let (outer, tag, current) = (event_tx.clone(), id.clone(), *generation);
                tokio::spawn(async move {
                    while let Some(event) = rx.recv().await {
                        if outer.send((tag.clone(), current, event)).await.is_err() {
                            break;
                        }
                    }
                });
                runtimes.insert(
                    id,
                    Running {
                        runtime,
                        generation: *generation,
                        dropped,
                    },
                );
            }
            Err(error) => state.apply_event_from(&id, BrokerEvent::Failed(format!("{error:#}"))),
        }
    }
}

async fn run(
    mut terminal: DefaultTerminal,
    config: AppConfig,
    broker_store: BrokerStore,
    brokers: Vec<String>,
    connect: bool,
) -> Result<()> {
    let mut state = UiState::new(config, broker_store, brokers.first().cloned());
    state.mark_brokers(&brokers);
    let (event_tx, mut event_rx) = mpsc::channel::<TaggedEvent>(EVENT_QUEUE_CAPACITY);
    let (test_tx, mut test_rx) = mpsc::channel::<(String, Result<(), String>)>(8);
    let mut runtimes = Runtimes::new();
    let mut generation = 0_u64;
    let mut logger = MqttLogger::new(state.log_format().clone());
    match state.startup_connect(connect) {
        UiAction::Connect(profile) => {
            start_connections(
                vec![profile],
                &mut state,
                &mut runtimes,
                &mut generation,
                &event_tx,
                &mut event_rx,
                &mut logger,
            )
            .await;
        }
        UiAction::ConnectMany(profiles) => {
            start_connections(
                profiles,
                &mut state,
                &mut runtimes,
                &mut generation,
                &event_tx,
                &mut event_rx,
                &mut logger,
            )
            .await;
        }
        _ => {}
    }
    let mut last_draw = Instant::now();
    let mut window_title = String::new();
    loop {
        // Bounded per iteration so a message flood cannot starve input handling and drawing.
        let mut handled = 0;
        while handled < MAX_EVENTS_PER_ITERATION {
            let Ok((id, event_generation, event)) = event_rx.try_recv() else {
                break;
            };
            handled += 1;
            // Events of a connection that was replaced meanwhile are stale.
            if runtimes
                .get(&id)
                .is_none_or(|running| running.generation != event_generation)
            {
                continue;
            }
            if matches!(event, BrokerEvent::Failed(_))
                && let Some(finished) = runtimes.remove(&id)
            {
                finished.runtime.stop().await;
            }
            deliver(&mut state, &mut logger, &id, event);
        }
        if state.log_directory().is_some() {
            if let Err(error) = logger.flush() {
                logger.close();
                state.logging_failed(&format!("{error:#}"));
            }
        } else {
            logger.close();
        }
        while let Ok((id, result)) = test_rx.try_recv() {
            state.apply_test_result(&id, result);
        }
        if let Some((broker, requests)) = state.replay_batch(Instant::now()) {
            let total = requests.len();
            for (done, request) in requests.into_iter().enumerate() {
                let result = match runtimes.get(&broker) {
                    Some(active) => active.runtime.publish(request),
                    None => Err(anyhow::anyhow!("not connected")),
                };
                if let Err(error) = result {
                    state.replay_failed(total - done, &error);
                    break;
                }
            }
        }
        let dropped = runtimes
            .values()
            .map(|running| running.dropped.load(Ordering::Relaxed))
            .sum();
        if dropped != state.dropped_messages {
            state.dropped_messages = dropped;
            state.mark_dirty();
        }
        state.tick(Instant::now());
        let title = state.window_title();
        if title != window_title {
            let _ = crossterm::execute!(io::stdout(), crossterm::terminal::SetTitle(&title));
            window_title = title;
        }
        if last_draw.elapsed() >= FRAME_INTERVAL && state.take_redraw() {
            terminal.draw(|frame| ui::render(frame, &mut state))?;
            last_draw = Instant::now();
        }
        // Keep draining without sleeping while a burst is still queued.
        let wait = match handled {
            0 if state.replay_active() => REPLAY_POLL_INTERVAL,
            0 => IDLE_POLL_INTERVAL,
            MAX_EVENTS_PER_ITERATION => Duration::ZERO,
            _ => Duration::from_millis(1),
        };
        if !event::poll(wait)? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match ui::handle_key_event(&mut state, key) {
                    UiAction::Quit => break,
                    UiAction::Connect(profile) => {
                        start_connections(
                            vec![profile],
                            &mut state,
                            &mut runtimes,
                            &mut generation,
                            &event_tx,
                            &mut event_rx,
                            &mut logger,
                        )
                        .await;
                    }
                    UiAction::ConnectMany(profiles) => {
                        start_connections(
                            profiles,
                            &mut state,
                            &mut runtimes,
                            &mut generation,
                            &event_tx,
                            &mut event_rx,
                            &mut logger,
                        )
                        .await;
                    }
                    UiAction::Disconnect => {
                        stop_runtimes(&mut runtimes, &mut event_rx, &mut state, &mut logger).await;
                        state.mark_disconnected();
                    }
                    UiAction::TestBroker(profile) => {
                        let store = state.broker_store.clone();
                        let results = test_tx.clone();
                        let id = profile.id.clone();
                        tokio::spawn(async move {
                            let outcome = check_profile(store, profile)
                                .await
                                .map_err(|error| format!("{error:#}"));
                            let _ = results.send((id, outcome)).await;
                        });
                    }
                    UiAction::Publish(broker, request) => {
                        let result = match runtimes.get(&broker) {
                            Some(active) => active
                                .runtime
                                .publish(request.clone())
                                .map_err(|error| format!("{error:#}")),
                            None => Err("not connected".to_owned()),
                        };
                        state.report_publish(&broker, &request, result);
                    }
                    UiAction::Subscription(broker, change) => {
                        let result = match (runtimes.get(&broker), &change) {
                            (Some(active), ui::SubscriptionChange::Subscribe { topic, qos }) => {
                                active.runtime.subscribe(topic, *qos)
                            }
                            (Some(active), ui::SubscriptionChange::Unsubscribe { topic }) => {
                                active.runtime.unsubscribe(topic)
                            }
                            (Some(active), ui::SubscriptionChange::Exclude { topic }) => {
                                active.runtime.exclude(topic)
                            }
                            (Some(active), ui::SubscriptionChange::Unexclude { topic }) => {
                                active.runtime.unexclude(topic);
                                Ok(())
                            }
                            (None, _) => Ok(()),
                        };
                        if let Err(error) = result {
                            state.report_subscription_failure(format!("{error:#}"));
                        }
                    }
                    UiAction::None => {}
                }
                if let Some(request) = state.take_clipboard_request() {
                    let (text, error) = copy_to_clipboard(&request)?;
                    state.show_notice(text, error);
                }
                if state.take_paste_request() {
                    match clipboard::paste_native() {
                        Ok(text) => ui::handle_paste(&mut state, &text),
                        Err(error) => state.show_notice(
                            format!("Cannot read the clipboard: {error}. Paste with your terminal's own shortcut (often Ctrl+Shift+V)."),
                            true,
                        ),
                    }
                }
            }
            Event::Resize(..) => state.mark_dirty(),
            Event::Paste(text) => ui::handle_paste(&mut state, &text),
            _ => {}
        }
    }
    stop_runtimes(&mut runtimes, &mut event_rx, &mut state, &mut logger).await;
    logger.close();
    Ok(())
}
