use beads::cli::commands;
use beads::cli::{Cli, Commands, OutputFormat};
use beads::config;
use beads::logging::init_logging;
use beads::output::OutputContext;
use beads::sync::{
    auto_flush, auto_import_if_stale, auto_import_probe, auto_import_probe_refreshing_witnesses,
};
use beads::{BeadsError, Result, StructuredError};
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;
use std::ffi::OsStr;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(not(test))]
const DISABLE_READ_ONLY_FAST_OPEN_ENV: &str = "BR_DISABLE_READ_ONLY_FAST_OPEN";

/// Parses the CLI, adding an explanation clap cannot give on its own.
///
/// bds-04l.12. `Cli::parse()` would print clap's message and exit; this
/// reproduces that exactly (`Error::print` already routes `--help`/`--version`
/// to stdout with exit 0, and everything else to stderr with exit 2) and then
/// appends `hyphen_value_hint` when it applies -- clap's own
/// `unexpected argument '- ' found` never names the flag that swallowed the
/// value.
fn parse_cli_or_exit() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            let argv: Vec<String> = std::env::args().collect();
            if let Some(hint) = beads::cli::hyphen_value_hint(err.kind(), &argv) {
                eprintln!("{hint}\n");
            }
            std::process::exit(err.exit_code());
        }
    }
}

#[allow(clippy::too_many_lines)]
fn main() {
    CompleteEnv::with_factory(Cli::command).complete();

    // Install SIGINT/SIGTERM/SIGHUP handlers before any storage opens so
    // an interrupt during a long-running command unwinds through main
    // and lets `SqliteStorage::Drop` flush the WAL (#270). The handler
    // is process-global and idempotent, so calling it from clap's
    // completion subprocess (above) would also be safe.
    beads::shutdown::install();

    let cli = parse_cli_or_exit();
    let json_error_mode = should_render_errors_as_json(&cli);
    let color_error_mode = should_color_human_errors_for_cli(&cli);
    let output_ctx = OutputContext::from_args(&cli);
    let is_mutating = is_mutating_command(&cli.command);
    let command_supports_auto_import = should_auto_import(&cli.command);

    // Initialize logging
    if let Err(e) = init_logging(cli.verbose, cli.quiet, None) {
        eprintln!("Failed to initialize logging: {e}");
    }

    let mut overrides = build_cli_overrides(&cli);

    // Phase 1: Startup & Discovery (One-time)
    let mut ctx = match StartupContext::init(&overrides) {
        Ok(ctx) => ctx,
        Err(e) => {
            if command_supports_auto_import {
                handle_error(&e, json_error_mode, color_error_mode);
            }
            StartupContext::empty(overrides.clone())
        }
    };

    let storage_enabled = ctx.is_initialized() && !ctx.no_db();
    let should_auto_import_now =
        command_supports_auto_import && !cli.allow_stale && !ctx.no_auto_import();
    let should_auto_flush_now = is_mutating && !ctx.no_auto_flush();
    let needs_preopened_storage_context = should_auto_import_now || should_auto_flush_now;
    let should_preopen_storage =
        should_preopen_storage(storage_enabled, needs_preopened_storage_context);
    let command_needs_write_lock = needs_write_lock(&cli.command);

    // Phase 1.5: Acquire exclusive write lock before any DB-family open that
    // may apply schema, recover, quarantine sidecars, write metadata, or read
    // from the previous engine while another process is in a write transaction.
    //
    // Issue #243: the previous engine deadlocked when multiple processes attempt
    // concurrent writes to the same database file. Serialize all mutating
    // operations through a blocking flock on `.beads/.write.lock`. Normal
    // storage open is not guaranteed read-only in recovery/schema paths, so
    // DB-family commands keep the advisory lock unless they can first use the
    // current-schema read-only fast-open path. Fast-open misses reacquire the
    // same write lock inside config before any recovery or writable fallback.
    let write_lock =
        if should_acquire_startup_write_lock(
            command_needs_write_lock,
            should_preopen_storage,
            ctx.overrides.read_only_fast_open,
        ) && ctx.is_initialized()
        {
            let lock_timeout = ctx.write_lock_timeout();
            match ctx.beads_dir.as_deref().map(|beads_dir| {
                beads::sync::blocking_write_lock_with_timeout(beads_dir, lock_timeout)
            }) {
                Some(Ok(lock)) => Some(lock),
                Some(Err(e)) => handle_error(&e, json_error_mode, color_error_mode),
                None => None,
            }
        } else {
            None
        };
    if write_lock.is_some() {
        overrides
            .held_write_lock_beads_dir
            .clone_from(&ctx.beads_dir);
    }

    // Phase 2: Open Storage (One-time)
    let mut storage_result = if should_preopen_storage {
        match open_storage_from_ctx(&mut ctx, write_lock.is_some()) {
            Ok(res) => Some(res),
            Err(e) => {
                if should_auto_import_now {
                    handle_error(&e, json_error_mode, color_error_mode);
                }
                None
            }
        }
    } else {
        None
    };

    // Phase 3: Auto-Import. Normal staleness probes can opportunistically
    // refresh JSONL witness metadata. Read-only startup probes skip that
    // refresh and reopen writable storage only when an import is actually
    // needed.
    if let Some(paths) = ctx.paths.as_ref()
        && should_auto_import_now
        && storage_result.is_some()
    {
        let allow_external_jsonl = config::implicit_external_jsonl_allowed(
            &paths.beads_dir,
            &paths.db_path,
            &paths.jsonl_path,
        );
        let mut auto_import_write_lock = None;
        if !ctx.overrides.read_only_fast_open && write_lock.is_none() {
            let lock_timeout = ctx.write_lock_timeout();
            auto_import_write_lock = match ctx.beads_dir.as_deref().map(|beads_dir| {
                beads::sync::blocking_write_lock_with_timeout(beads_dir, lock_timeout)
            }) {
                Some(Ok(lock)) => Some(lock),
                Some(Err(e)) => handle_error(&e, json_error_mode, color_error_mode),
                None => None,
            };
        }
        let should_attempt_auto_import = {
            match storage_result.as_mut() {
                Some(res) if ctx.overrides.read_only_fast_open => auto_import_probe(
                    &res.storage,
                    &paths.beads_dir,
                    &paths.jsonl_path,
                    allow_external_jsonl,
                )
                .unwrap_or(true),
                Some(res) => auto_import_probe_refreshing_witnesses(
                    &mut res.storage,
                    &paths.beads_dir,
                    &paths.jsonl_path,
                    allow_external_jsonl,
                )
                .unwrap_or(true),
                None => false,
            }
        };

        if should_attempt_auto_import {
            if ctx.overrides.read_only_fast_open && write_lock.is_none() {
                let lock_timeout = ctx.write_lock_timeout();
                auto_import_write_lock = match ctx.beads_dir.as_deref().map(|beads_dir| {
                    beads::sync::blocking_write_lock_with_timeout(beads_dir, lock_timeout)
                }) {
                    Some(Ok(lock)) => Some(lock),
                    Some(Err(e)) => handle_error(&e, json_error_mode, color_error_mode),
                    None => None,
                };
            }

            if ctx.overrides.read_only_fast_open {
                let mut writable_overrides = ctx.overrides.clone();
                writable_overrides.read_only_fast_open = false;
                drop(storage_result.take());
                match config::open_storage_with_cli(&paths.beads_dir, &writable_overrides) {
                    Ok(writable_res) => storage_result = Some(writable_res),
                    Err(e) => handle_error(&e, json_error_mode, color_error_mode),
                }
            }

            let _ = auto_import_write_lock.as_ref();
            let sync_lock = match ctx.beads_dir.as_deref().map(beads::sync::try_sync_lock) {
                Some(Ok(Some(lock))) => Some(lock),
                Some(Ok(None)) => {
                    tracing::debug!("Auto-import skipped because .sync.lock is held");
                    None
                }
                Some(Err(e)) => handle_error(&e, json_error_mode, color_error_mode),
                None => None,
            };
            if sync_lock.is_some()
                && let Some(res) = storage_result.as_mut()
            {
                let expected_prefix = match resolve_auto_import_expected_prefix(res, &ctx.overrides)
                {
                    Ok(prefix) => Some(prefix),
                    Err(e) => {
                        handle_error(&e, json_error_mode, color_error_mode);
                    }
                };
                let outcome = auto_import_if_stale(
                    &mut res.storage,
                    &paths.beads_dir,
                    &paths.jsonl_path,
                    expected_prefix.as_deref(),
                    allow_external_jsonl,
                    false,
                    false,
                );
                if let Err(e) = outcome {
                    handle_error(&e, json_error_mode, color_error_mode);
                }
            }
            // sync_lock drops here, releasing the advisory lock before command execution
        }
    }

    // Phase 4: Command Execution
    let result = match cli.command {
        Commands::Init {
            prefix,
            force,
            backend: _,
        } => commands::init::execute(prefix, force, None, &output_ctx),
        Commands::Create(args) => {
            execute_create_command(&args, &overrides, &output_ctx, &mut storage_result)
        }
        Commands::Update(args) => commands::update::execute(&args, &overrides, &output_ctx),
        Commands::Delete(args) => {
            commands::delete::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::List(args) => {
            if let Some(res) = storage_result.as_ref() {
                commands::list::execute_with_storage(&args, &overrides, &output_ctx, res)
            } else {
                commands::list::execute(&args, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Comments(args) => {
            if let (Some(res), Some(beads_dir)) = (storage_result.as_ref(), ctx.beads_dir.as_ref())
            {
                match commands::comments::execute_with_storage_ctx(
                    &args,
                    cli.json,
                    &overrides,
                    &output_ctx,
                    beads_dir,
                    res,
                ) {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        commands::comments::execute(&args, cli.json, &overrides, &output_ctx)
                    }
                    Err(err) => Err(err),
                }
            } else {
                commands::comments::execute(&args, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Search(args) => {
            if let Some(res) = storage_result.as_ref() {
                commands::search::execute_with_storage_ctx(&args, &overrides, &output_ctx, res)
            } else {
                commands::search::execute(&args, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Show(args) => {
            if let (Some(res), Some(beads_dir)) = (storage_result.as_ref(), ctx.beads_dir.as_ref())
            {
                commands::show::execute_with_storage_ctx(
                    &args,
                    &overrides,
                    &output_ctx,
                    beads_dir,
                    res,
                )
            } else {
                commands::show::execute(&args, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Close(args) => {
            commands::close::execute_cli(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::Reopen(args) => {
            commands::reopen::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::Detach(args) => {
            commands::detach::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::Rename(args) => {
            commands::rename::execute(&args, cli.json, &overrides, &output_ctx)
        }
        Commands::Dep { command } => {
            if let (Some(res), Some(beads_dir)) = (storage_result.as_ref(), ctx.beads_dir.as_ref())
            {
                match commands::dep::execute_with_storage_ctx(
                    &command,
                    cli.json,
                    &overrides,
                    &output_ctx,
                    beads_dir,
                    res,
                ) {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        commands::dep::execute(&command, cli.json, &overrides, &output_ctx)
                    }
                    Err(err) => Err(err),
                }
            } else {
                commands::dep::execute(&command, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Epic { command } => {
            if let Some(res) = storage_result.as_ref() {
                match commands::epic::execute_with_storage_ctx(
                    &command,
                    &overrides,
                    &output_ctx,
                    res,
                ) {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        commands::epic::execute(&command, cli.json, &overrides, &output_ctx)
                    }
                    Err(err) => Err(err),
                }
            } else {
                commands::epic::execute(&command, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Label { command } => {
            if let Some(res) = storage_result.as_ref() {
                match commands::label::execute_with_storage(
                    &command,
                    cli.json,
                    &output_ctx,
                    &res.storage,
                ) {
                    Ok(true) => Ok(()),
                    Ok(false) => {
                        commands::label::execute(&command, cli.json, &overrides, &output_ctx)
                    }
                    Err(err) => Err(err),
                }
            } else {
                commands::label::execute(&command, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Stale(args) => storage_result.as_ref().map_or_else(
            || commands::stale::execute(&args, &overrides, &output_ctx),
            |res| commands::stale::execute_with_storage(&args, &output_ctx, &res.storage),
        ),
        Commands::Ready(args) => {
            if let (Some(res), Some(beads_dir)) = (storage_result.as_ref(), ctx.beads_dir.as_ref())
            {
                commands::ready::execute_with_storage_ctx(
                    &args,
                    &overrides,
                    &output_ctx,
                    beads_dir,
                    res,
                )
            } else {
                commands::ready::execute(&args, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Blocked(args) => {
            if let (Some(res), Some(beads_dir)) = (storage_result.as_ref(), ctx.beads_dir.as_ref())
            {
                commands::blocked::execute_with_storage_ctx(
                    &args,
                    &overrides,
                    &output_ctx,
                    beads_dir,
                    res,
                )
            } else {
                commands::blocked::execute(&args, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Sync(args) => commands::sync::execute(
            &args,
            cli.json,
            &overrides,
            &output_ctx,
            write_lock.is_some(),
        ),
        Commands::Info(args) => commands::info::execute(&args, &overrides, &output_ctx),
        Commands::Version(args) => commands::version::execute(&args, &output_ctx),
        Commands::Completions(args) => commands::completions::execute(&args, &output_ctx),
        Commands::Stats(args) => {
            if let (Some(res), Some(beads_dir)) = (storage_result.as_ref(), ctx.beads_dir.as_ref())
            {
                commands::stats::execute_with_storage_ctx(
                    &args,
                    &overrides,
                    &output_ctx,
                    beads_dir,
                    res,
                )
            } else {
                commands::stats::execute(&args, cli.json, &overrides, &output_ctx)
            }
        }
        Commands::Config { command } => {
            commands::config::execute(&command, cli.json, &overrides, &output_ctx)
        }
        Commands::History(args) => commands::history::execute(args, &overrides, &output_ctx),
    };

    // Handle command result
    if let Err(e) = result {
        handle_error(&e, json_error_mode, color_error_mode);
    }

    // Cooperative shutdown: if a SIGINT/SIGTERM/SIGHUP arrived while
    // the command was executing, skip the auto-flush phase and let
    // every local — including `storage_result` — drop on the way out
    // of `main`, so `SqliteStorage::Drop` checkpoints the WAL before
    // the process exits (#270).
    if let Some(exit_code) = beads::shutdown::exit_code() {
        drop(storage_result);
        drop(write_lock);
        std::process::exit(exit_code);
    }

    // Phase 5: Auto-Flush (with advisory flock to serialize concurrent access)
    if is_mutating
        && !ctx.no_auto_flush()
        && let (Some(res), Some(paths)) = (storage_result.as_mut(), ctx.paths.as_ref())
    {
        let sync_lock = match beads::sync::try_sync_lock(&paths.beads_dir) {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => {
                let err = BeadsError::Config(format!(
                    "Automatic JSONL export skipped because sync lock at {} is held by another process",
                    paths.beads_dir.join(".sync.lock").display()
                ));
                commands::report_auto_flush_failure(
                    &output_ctx,
                    &paths.beads_dir,
                    &paths.jsonl_path,
                    &err,
                );
                None
            }
            Err(e) => {
                commands::report_auto_flush_failure(
                    &output_ctx,
                    &paths.beads_dir,
                    &paths.jsonl_path,
                    &e,
                );
                None
            }
        };

        if let Some(_sync_lock) = sync_lock
            && let Err(e) = auto_flush(
                &mut res.storage,
                &paths.beads_dir,
                &paths.jsonl_path,
                config::implicit_external_jsonl_allowed(
                    &paths.beads_dir,
                    &paths.db_path,
                    &paths.jsonl_path,
                ),
            )
        {
            commands::report_auto_flush_failure(
                &output_ctx,
                &paths.beads_dir,
                &paths.jsonl_path,
                &e,
            );
        }
    }

    if let Some(err) = beads::output::take_output_serialization_failure() {
        std::process::exit(err.exit_code());
    }

    // A command emitted its normal output and any auto-flush has now completed,
    // but detected a condition that must surface to scripted callers via a
    // non-zero exit code (e.g. `dep cycles` with cycles present, or `create -f`
    // that dropped declared dependency edges) — see #368. Drop storage first so
    // `SqliteStorage::Drop` checkpoints the WAL before the process exits (#270).
    if let Some(exit_code) = beads::output::take_pending_exit_code() {
        drop(storage_result);
        drop(write_lock);
        std::process::exit(exit_code);
    }
}

struct StartupContext {
    overrides: config::CliOverrides,
    startup: Option<config::StartupConfig>,
    beads_dir: Option<PathBuf>,
    paths: Option<config::ConfigPaths>,
    config: Option<config::ConfigLayer>,
}

impl StartupContext {
    fn init(overrides: &config::CliOverrides) -> Result<Self> {
        let beads_dir = config::discover_beads_dir_with_cli(overrides)?;
        let startup = config::load_startup_config_with_paths(&beads_dir, overrides.db.as_ref())?;

        // Merge startup config with CLI overrides to form the effective bootstrap config
        let mut final_config = startup.merged_config.clone();
        final_config.merge_from(&overrides.as_layer());
        let paths = startup.paths.clone();

        Ok(Self {
            overrides: overrides.clone(),
            startup: Some(startup),
            beads_dir: Some(beads_dir),
            paths: Some(paths),
            config: Some(final_config),
        })
    }

    fn empty(overrides: config::CliOverrides) -> Self {
        Self {
            overrides,
            startup: None,
            beads_dir: None,
            paths: None,
            config: None,
        }
    }

    fn is_initialized(&self) -> bool {
        self.beads_dir.is_some()
    }

    fn no_db(&self) -> bool {
        self.config
            .as_ref()
            .and_then(config::no_db_from_layer)
            .unwrap_or(false)
    }

    fn no_auto_import(&self) -> bool {
        self.config
            .as_ref()
            .and_then(config::no_auto_import_from_layer)
            .unwrap_or(false)
    }

    fn no_auto_flush(&self) -> bool {
        self.config
            .as_ref()
            .and_then(config::no_auto_flush_from_layer)
            .unwrap_or(false)
    }

    fn configured_write_lock_timeout(&self) -> Option<u64> {
        self.config
            .as_ref()
            .and_then(config::lock_timeout_from_layer)
            .or(self.overrides.lock_timeout)
    }

    fn write_lock_timeout(&self) -> Option<u64> {
        self.configured_write_lock_timeout()
            .or(Some(beads::sync::default_write_lock_timeout_ms()))
    }
}

fn open_storage_from_ctx(
    ctx: &mut StartupContext,
    write_lock_already_held: bool,
) -> Result<config::OpenStorageResult> {
    let startup = ctx.startup.take().ok_or(BeadsError::NotInitialized)?;
    if write_lock_already_held {
        config::open_storage_with_startup_config_under_write_lock(startup, &ctx.overrides, false)
    } else {
        config::open_storage_with_startup_config(startup, &ctx.overrides, false)
    }
}

fn resolve_auto_import_expected_prefix(
    storage_result: &config::OpenStorageResult,
    cli: &config::CliOverrides,
) -> Result<String> {
    let layer = storage_result.load_config(cli)?;
    Ok(config::id_config_from_layer(&layer).prefix)
}

fn execute_create_command(
    args: &beads::cli::CreateArgs,
    overrides: &config::CliOverrides,
    output_ctx: &OutputContext,
    storage_result: &mut Option<config::OpenStorageResult>,
) -> Result<()> {
    commands::create::execute_with_storage(args, overrides, output_ctx, storage_result.take())
}

const fn should_preopen_storage(
    storage_enabled: bool,
    needs_preopened_storage_context: bool,
) -> bool {
    storage_enabled && needs_preopened_storage_context
}

const fn sync_mode_opens_storage(args: &beads::cli::SyncArgs) -> bool {
    args.flush_only || args.import_only || args.merge || args.status
}

const fn should_acquire_startup_write_lock(
    command_needs_write_lock: bool,
    should_preopen_storage: bool,
    read_only_fast_open: bool,
) -> bool {
    !read_only_fast_open && (command_needs_write_lock || should_preopen_storage)
}

/// Determine if a command potentially mutates data and triggers auto-flush.
const fn is_mutating_command(cmd: &Commands) -> bool {
    match cmd {
        Commands::Create(_)
        | Commands::Update(_)
        | Commands::Delete(_)
        | Commands::Close(_)
        | Commands::Reopen(_)
        | Commands::Detach(_)
        // `--dry-run` writes nothing, but this decides auto-flush for the whole
        // command before the args are read; treating rename as mutating costs a
        // no-op flush on a dry run and never misses a real one.
        | Commands::Rename(_) => true,
        Commands::Dep { command } => matches!(
            command,
            beads::cli::DepCommands::Add(_)
                | beads::cli::DepCommands::Import(_)
                | beads::cli::DepCommands::Remove(_)
        ),
        Commands::Label { command } => matches!(
            command,
            beads::cli::LabelCommands::Add(_)
                | beads::cli::LabelCommands::Remove(_)
                | beads::cli::LabelCommands::Rename(_)
        ),
        Commands::Comments(args) => matches!(
            args.command.as_ref(),
            Some(beads::cli::CommentCommands::Add(_))
        ),
        Commands::Epic { command } => matches!(
            command,
            beads::cli::EpicCommands::CloseEligible(args) if !args.dry_run
        ),
        _ => false,
    }
}

/// Determine if a command must hold `.write.lock` for its whole execution.
const fn needs_write_lock(cmd: &Commands) -> bool {
    if is_mutating_command(cmd) {
        return true;
    }
    match cmd {
        // Every command in this arm can open or snapshot the SQLite DB family.
        // Serialize them before command execution so `--allow-stale`,
        // `--no-auto-import`, and direct command-local open paths do not bypass
        // the startup lock that protects recovery/schema/default metadata work.
        //
        // Every explicit DB-backed sync mode must open storage inside
        // `sync::execute`.
        // `--flush-only` looks like a "just rewrite JSONL" path but also calls
        // `finalize_export` inside a `with_write_transaction`, updating dirty
        // flags, export hashes, and metadata (jsonl_content_hash,
        // last_export_time, needs_flush). Without the `.write.lock`, a
        // concurrent `br sync --flush-only` racing with another process's
        // auto-flush (or a second `--flush-only`) can trip the previous engine's
        // concurrent-write deadlock that this lock was specifically added
        // to prevent (issue #243). `--status` only renders status after open,
        // but opening storage can still apply schema/runtime defaults or
        // recover the DB family, so it must also serialize before open.
        // `br sync --witness` hashes JSONL and returns before opening SQLite, so
        // it also should not block behind DB writers. Bare `br sync` is invalid
        // and fails validation before storage open, so it should not block on
        // `.write.lock` just to report an argument error.
        Commands::List(_)
        | Commands::Show(_)
        | Commands::Search(_)
        | Commands::Ready(_)
        | Commands::Blocked(_)
        | Commands::Stale(_)
        | Commands::Stats(_)
        | Commands::Comments(_)
        | Commands::Dep { .. }
        | Commands::Label { .. }
        | Commands::Epic { .. }
        | Commands::Info(_)
        | Commands::Init { .. } => true,
        Commands::Sync(args) => sync_mode_opens_storage(args),
        Commands::Config { command } => !matches!(
            command,
            beads::cli::ConfigCommands::Path | beads::cli::ConfigCommands::Edit
        ),
        Commands::History(args) => matches!(
            args.command,
            Some(
                beads::cli::HistoryCommands::Restore { .. }
                    | beads::cli::HistoryCommands::Prune { .. }
            )
        ),
        _ => false,
    }
}

const fn should_auto_import(cmd: &Commands) -> bool {
    match cmd {
        Commands::List(_)
        | Commands::Show(_)
        | Commands::Search(_)
        | Commands::Ready(_)
        | Commands::Blocked(_)
        | Commands::Stale(_)
        | Commands::Stats(_)
        | Commands::Create(_)
        | Commands::Update(_)
        | Commands::Delete(_)
        | Commands::Close(_)
        | Commands::Reopen(_)
        | Commands::Detach(_)
        | Commands::Rename(_)
        | Commands::Comments(_)
        | Commands::Dep { .. }
        | Commands::Label { .. }
        | Commands::Epic { .. } => true,

        Commands::Init { .. }
        | Commands::Sync(_)
        | Commands::Info(_)
        | Commands::Version(_)
        | Commands::Completions(_)
        | Commands::Config { .. }
        | Commands::History(_) => false,
    }
}

const fn supports_read_only_fast_open(cmd: &Commands) -> bool {
    match cmd {
        Commands::Sync(args) => args.status,
        Commands::Stats(_)
        | Commands::List(_)
        | Commands::Show(_)
        | Commands::Search(_)
        | Commands::Ready(_)
        | Commands::Blocked(_)
        | Commands::Stale(_)
        | Commands::Comments(beads::cli::CommentsArgs {
            command: None | Some(beads::cli::CommentCommands::List(_)),
            ..
        })
        | Commands::Epic {
            command: beads::cli::EpicCommands::Status(_),
        } => true,
        Commands::Dep { command } => is_read_only_dep_command(command),
        Commands::Label { command } => is_read_only_label_listing(command),
        _ => false,
    }
}

const fn is_read_only_dep_command(command: &beads::cli::DepCommands) -> bool {
    match command {
        beads::cli::DepCommands::List(_)
        | beads::cli::DepCommands::Tree(_)
        | beads::cli::DepCommands::Cycles(_) => true,
        beads::cli::DepCommands::Add(_)
        | beads::cli::DepCommands::Import(_)
        | beads::cli::DepCommands::Remove(_) => false,
    }
}

const fn is_read_only_label_listing(command: &beads::cli::LabelCommands) -> bool {
    match command {
        beads::cli::LabelCommands::ListAll
        | beads::cli::LabelCommands::List(beads::cli::LabelListArgs { issue: None }) => true,
        beads::cli::LabelCommands::Add(_)
        | beads::cli::LabelCommands::Remove(_)
        | beads::cli::LabelCommands::List(_)
        | beads::cli::LabelCommands::Rename(_) => false,
    }
}

fn command_requested_output_format(cmd: &Commands) -> Option<OutputFormat> {
    match cmd {
        Commands::List(args) => args.format,
        Commands::Search(args) => args.filters.format,
        Commands::Show(args) => args.format.map(Into::into),
        Commands::Ready(args) => args.format.map(Into::into),
        Commands::Blocked(args) => args.format.map(Into::into),
        Commands::Stats(args) => args.format.map(Into::into),
        Commands::Dep { command } => match command {
            beads::cli::DepCommands::List(args) => args.format.map(Into::into),
            beads::cli::DepCommands::Tree(_)
            | beads::cli::DepCommands::Add(_)
            | beads::cli::DepCommands::Import(_)
            | beads::cli::DepCommands::Remove(_)
            | beads::cli::DepCommands::Cycles(_) => None,
        },
        _ => None,
    }
}

fn should_render_errors_as_json_with_env(
    cli: &Cli,
    env_output_format: Option<OutputFormat>,
) -> bool {
    cli.json
        || matches!(
            command_requested_output_format(&cli.command).or(env_output_format),
            Some(OutputFormat::Json)
        )
}

fn should_render_errors_as_json(cli: &Cli) -> bool {
    should_render_errors_as_json_with_env(cli, OutputFormat::from_env())
}

const fn should_color_human_errors(
    no_color_flag: bool,
    no_color_env_present: bool,
    stderr_is_terminal: bool,
) -> bool {
    !no_color_flag && !no_color_env_present && stderr_is_terminal
}

fn should_color_human_errors_for_cli(cli: &Cli) -> bool {
    should_color_human_errors(
        cli.no_color,
        std::env::var_os("NO_COLOR").is_some(),
        io::stderr().is_terminal(),
    )
}

/// Handle errors with structured output support.
/// Read every issue ID out of the workspace's JSONL export.
///
/// The JSONL rather than the database, for two reasons. It needs no lock,
/// which matters on a path that is already failing and may be failing
/// *because* the database is busy; and it is the same source the shell
/// completers read, so a suggestion cannot name an ID that TAB would not have
/// offered. The cost is that an issue created since the last export is not a
/// candidate -- acceptable for a hint, and the alternative is opening a
/// database to decorate an error message.
///
/// Returns an empty vector on any failure. Every caller degrades to the
/// unsuggested error, which is the behaviour that shipped before.
fn known_issue_ids() -> Vec<String> {
    use std::io::BufRead;

    let Ok(beads_dir) = config::discover_beads_dir(None) else {
        return Vec::new();
    };
    let Ok(paths) = config::resolve_paths(&beads_dir, None) else {
        return Vec::new();
    };
    let Ok(file) = std::fs::File::open(&paths.jsonl_path) else {
        return Vec::new();
    };

    std::io::BufReader::new(file)
        .lines()
        // Spelled out: bare `Result::ok` would resolve to beads' own `Result`
        // alias, which this module imports.
        .map_while(std::result::Result::ok)
        .filter_map(|line| {
            serde_json::from_str::<serde_json::Value>(&line)
                .ok()?
                .get("id")?
                .as_str()
                .map(ToString::to_string)
        })
        .collect()
}

fn handle_error(err: &BeadsError, json_mode: bool, color_mode: bool) -> ! {
    let structured = StructuredError::from_error(err);
    // bds-k7e. Only ISSUE_NOT_FOUND is enriched, and `with_id_suggestions` is
    // a no-op for anything else -- so the JSONL is read only when a lookup has
    // actually failed, never on the success path.
    let structured = if structured.code == beads::ErrorCode::IssueNotFound {
        structured.with_id_suggestions(&known_issue_ids())
    } else {
        structured
    };
    let exit_code = structured.code.exit_code();

    if json_mode {
        // #336: In `--json` mode, route the structured JSON error envelope to
        // STDOUT (where success JSON already goes) so scripted callers read ONE
        // clean, parseable stream. tracing/log lines stay on stderr (see
        // `logging::init_logging`, which writes to `std::io::stderr`), so the
        // stdout JSON is never interleaved with diagnostic noise.
        let json = structured.to_json();
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_else(|_| json.to_string())
        );
    } else {
        // Human mode: errors stay on stderr so stdout remains usable for the
        // command's normal (non-error) output and pipelines.
        eprintln!("{}", structured.to_human(color_mode));
    }

    std::process::exit(exit_code);
}

fn build_cli_overrides(cli: &Cli) -> config::CliOverrides {
    let read_only_fast_open = !cli.no_db
        && cli.lock_timeout.is_none()
        && !read_only_fast_open_disabled_for_cli()
        && supports_read_only_fast_open(&cli.command)
        && cli.no_auto_import
        && cli.no_auto_flush;

    config::CliOverrides {
        db: cli.db.clone(),
        actor: cli.actor.clone(),
        identity: None,
        // Only set bool overrides when the CLI flag was explicitly provided.
        // Eagerly setting Some(false) would override config-file values with the
        // CLI default, preventing users from setting these via config.
        json: cli.json.then_some(true),
        display_color: if cli.no_color { Some(false) } else { None },
        quiet: cli.quiet.then_some(true),
        allow_stale: if cli.allow_stale { Some(true) } else { None },
        no_db: if cli.no_db { Some(true) } else { None },
        no_auto_flush: if cli.no_auto_flush { Some(true) } else { None },
        no_auto_import: if cli.no_auto_import { Some(true) } else { None },
        lock_timeout: cli.lock_timeout,
        held_write_lock_beads_dir: None,
        read_only_fast_open,
    }
}

#[cfg(not(test))]
fn read_only_fast_open_disabled_for_cli() -> bool {
    std::env::var_os(DISABLE_READ_ONLY_FAST_OPEN_ENV)
        .as_deref()
        .is_some_and(read_only_fast_open_disable_value_is_truthy)
}

#[cfg(test)]
const fn read_only_fast_open_disabled_for_cli() -> bool {
    false
}

fn read_only_fast_open_disable_value_is_truthy(value: &OsStr) -> bool {
    matches!(
        value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::fs;
    use tempfile::TempDir;

    fn make_create_args() -> beads::cli::CreateArgs {
        beads::cli::CreateArgs {
            title: Some("test-title".to_string()),
            title_flag: None,
            type_: None,
            priority: None,
            description: None,
            description_file: None,
            assignee: None,
            owner: None,
            labels: Vec::new(),
            parent: None,
            deps: Vec::new(),
            estimate: None,
            due: None,
            defer: None,
            external_ref: None,
            status: None,
            ephemeral: false,
            dry_run: false,
            silent: false,
            file: None,
        }
    }

    #[test]
    fn parse_global_flags_and_command() {
        let cli = Cli::parse_from(["br", "--json", "-vv", "list"]);
        assert!(cli.json);
        assert_eq!(cli.verbose, 2);
        assert!(!cli.quiet);
        assert!(matches!(cli.command, Commands::List(_)));
    }

    #[test]
    fn parse_create_title_positional() {
        let cli = Cli::parse_from(["br", "create", "FixBug"]);
        match cli.command {
            Commands::Create(args) => {
                assert_eq!(args.title.as_deref(), Some("FixBug"));
            }
            other => unreachable!("expected create command, got {other:?}"),
        }
    }

    #[test]
    fn human_error_color_respects_no_color_precedence() {
        assert!(
            should_color_human_errors(false, false, true),
            "interactive stderr should use color when no color controls are set"
        );
        assert!(
            !should_color_human_errors(true, false, true),
            "--no-color must suppress ANSI error output even on a TTY"
        );
        assert!(
            !should_color_human_errors(false, true, true),
            "NO_COLOR must suppress ANSI error output even on a TTY"
        );
        assert!(
            !should_color_human_errors(false, false, false),
            "non-terminal stderr should not receive ANSI error output"
        );
    }

    #[test]
    fn build_overrides_maps_flags() {
        let cli = Cli::parse_from([
            "br",
            "--json",
            "--no-color",
            "--allow-stale",
            "--no-db",
            "--no-auto-flush",
            "--lock-timeout",
            "2500",
            "list",
        ]);
        let overrides = build_cli_overrides(&cli);
        assert_eq!(overrides.json, Some(true));
        assert_eq!(overrides.display_color, Some(false));
        assert_eq!(overrides.allow_stale, Some(true));
        assert_eq!(overrides.no_db, Some(true));
        assert_eq!(overrides.no_auto_flush, Some(true));
        assert_eq!(overrides.lock_timeout, Some(2500));
    }

    #[test]
    fn build_overrides_omits_absent_startup_bool_flags() {
        let cli = Cli::parse_from(["br", "list"]);
        let overrides = build_cli_overrides(&cli);

        // Absent CLI bool flags must not produce Some(false) overrides — that
        // would silently clobber any config-file value (e.g. `sync.auto_flush:
        // false` would be ignored because the CLI's default `false` wins).
        assert_eq!(overrides.json, None);
        assert_eq!(overrides.quiet, None);
        assert_eq!(overrides.no_db, None);
        assert_eq!(overrides.no_auto_flush, None);
        assert_eq!(overrides.no_auto_import, None);
        assert_eq!(overrides.allow_stale, None);
    }

    #[test]
    fn read_only_fast_open_requires_explicit_stale_and_flush_opt_out() {
        let list = Cli::parse_from(["br", "list"]);
        assert!(!build_cli_overrides(&list).read_only_fast_open);

        let list_with_lock_timeout = Cli::parse_from(["br", "--lock-timeout", "50", "list"]);
        assert!(!build_cli_overrides(&list_with_lock_timeout).read_only_fast_open);

        let stats = Cli::parse_from(["br", "stats"]);
        assert!(!build_cli_overrides(&stats).read_only_fast_open);

        let stats_no_auto = Cli::parse_from(["br", "--no-auto-import", "--no-auto-flush", "stats"]);
        assert!(build_cli_overrides(&stats_no_auto).read_only_fast_open);

        let stats_no_activity = Cli::parse_from(["br", "stats", "--no-activity"]);
        assert!(!build_cli_overrides(&stats_no_activity).read_only_fast_open);

        let sync_status = Cli::parse_from(["br", "sync", "--status"]);
        assert!(!build_cli_overrides(&sync_status).read_only_fast_open);

        let sync_flush = Cli::parse_from(["br", "sync", "--flush-only"]);
        assert!(!build_cli_overrides(&sync_flush).read_only_fast_open);

        let sync_import = Cli::parse_from(["br", "sync", "--import-only"]);
        assert!(!build_cli_overrides(&sync_import).read_only_fast_open);

        let ready = Cli::parse_from(["br", "--no-auto-import", "--no-auto-flush", "ready"]);
        assert!(build_cli_overrides(&ready).read_only_fast_open);

        let comments_list = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "comments",
            "list",
            "bd-abc",
        ]);
        assert!(build_cli_overrides(&comments_list).read_only_fast_open);

        let comments_shorthand = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "comments",
            "bd-abc",
        ]);
        assert!(build_cli_overrides(&comments_shorthand).read_only_fast_open);

        let label_list_all = Cli::parse_from(["br", "label", "list-all"]);
        assert!(!build_cli_overrides(&label_list_all).read_only_fast_open);

        let label_list_unique = Cli::parse_from(["br", "label", "list"]);
        assert!(!build_cli_overrides(&label_list_unique).read_only_fast_open);

        let label_list_issue = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "label",
            "list",
            "bd-abc",
        ]);
        assert!(!build_cli_overrides(&label_list_issue).read_only_fast_open);

        let comments_no_auto_import =
            Cli::parse_from(["br", "--no-auto-import", "comments", "list", "bd-abc"]);
        assert!(!build_cli_overrides(&comments_no_auto_import).read_only_fast_open);

        let mutating = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "create",
            "write path",
        ]);
        assert!(!build_cli_overrides(&mutating).read_only_fast_open);

        let label_add = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "label",
            "add",
            "bd-abc",
            "--label",
            "bug",
        ]);
        assert!(!build_cli_overrides(&label_add).read_only_fast_open);

        let comments_add = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "comments",
            "add",
            "bd-abc",
            "write path",
        ]);
        assert!(!build_cli_overrides(&comments_add).read_only_fast_open);
    }

    #[test]
    fn read_only_fast_open_disable_env_parser_is_conservative() {
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(
                read_only_fast_open_disable_value_is_truthy(OsStr::new(value)),
                "{value:?} should disable read-only fast-open"
            );
        }

        for value in ["", "0", "false", "off", "no", "maybe"] {
            assert!(
                !read_only_fast_open_disable_value_is_truthy(OsStr::new(value)),
                "{value:?} should not disable read-only fast-open"
            );
        }
    }

    #[test]
    fn default_read_commands_do_not_fast_open_for_auto_import_probe() {
        let ready = Cli::parse_from(["br", "ready"]);
        assert!(!build_cli_overrides(&ready).read_only_fast_open);

        let blocked = Cli::parse_from(["br", "blocked"]);
        assert!(!build_cli_overrides(&blocked).read_only_fast_open);

        let show = Cli::parse_from(["br", "show", "br-123"]);
        assert!(!build_cli_overrides(&show).read_only_fast_open);

        let comments_list = Cli::parse_from(["br", "comments", "list", "br-123"]);
        assert!(!build_cli_overrides(&comments_list).read_only_fast_open);

        let search = Cli::parse_from(["br", "search", "needle"]);
        assert!(!build_cli_overrides(&search).read_only_fast_open);

        let stale = Cli::parse_from(["br", "stale"]);
        assert!(!build_cli_overrides(&stale).read_only_fast_open);

        let epic_status = Cli::parse_from(["br", "epic", "status"]);
        assert!(!build_cli_overrides(&epic_status).read_only_fast_open);

        let dep_tree = Cli::parse_from(["br", "dep", "tree", "br-123"]);
        assert!(!build_cli_overrides(&dep_tree).read_only_fast_open);

        let dep_list = Cli::parse_from(["br", "dep", "list", "br-123"]);
        assert!(!build_cli_overrides(&dep_list).read_only_fast_open);

        let dep_cycles = Cli::parse_from(["br", "dep", "cycles"]);
        assert!(!build_cli_overrides(&dep_cycles).read_only_fast_open);
    }

    #[test]
    fn read_only_fast_open_covers_epic_status_only() {
        let status = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "epic",
            "status",
        ]);
        assert!(build_cli_overrides(&status).read_only_fast_open);

        let close_eligible = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "epic",
            "close-eligible",
            "--dry-run",
        ]);
        assert!(!build_cli_overrides(&close_eligible).read_only_fast_open);
    }

    #[test]
    fn read_only_fast_open_covers_read_only_dep_commands() {
        let dep_tree = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "dep",
            "tree",
            "bd-abc",
        ]);
        assert!(build_cli_overrides(&dep_tree).read_only_fast_open);

        let dep_cycles =
            Cli::parse_from(["br", "--no-auto-import", "--no-auto-flush", "dep", "cycles"]);
        assert!(build_cli_overrides(&dep_cycles).read_only_fast_open);

        let dep_add = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "dep",
            "add",
            "bd-abc",
            "bd-def",
        ]);
        assert!(!build_cli_overrides(&dep_add).read_only_fast_open);

        let dep_import = Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "dep",
            "import",
            "edges.jsonl",
        ]);
        assert!(!build_cli_overrides(&dep_import).read_only_fast_open);
    }

    #[test]
    fn read_only_fast_open_defers_startup_write_lock_until_fallback() {
        assert!(
            !should_acquire_startup_write_lock(true, false, true),
            "read-only fast-open commands should try the current-schema read-only DB before joining the writer lock path"
        );
        assert!(
            !should_acquire_startup_write_lock(true, true, true),
            "explicit read-only fast-open probes defer the writer lock until writable fallback"
        );
        assert!(
            should_acquire_startup_write_lock(false, true, false),
            "non-fast-open DB-family commands must keep the startup lock"
        );
    }

    #[test]
    fn caller_write_lock_scope_is_path_specific() {
        let mut overrides = build_cli_overrides(&Cli::parse_from([
            "br",
            "--no-auto-import",
            "--no-auto-flush",
            "list",
        ]));
        let beads_dir = PathBuf::from("/tmp/beads/.beads");
        let other_dir = PathBuf::from("/tmp/other/.beads");

        overrides.held_write_lock_beads_dir = Some(beads_dir.clone());

        assert!(overrides.holds_write_lock_for(&beads_dir));
        assert!(!overrides.holds_write_lock_for(&other_dir));
    }

    #[test]
    fn help_includes_core_commands() {
        let help = Cli::command().render_help().to_string();
        assert!(help.contains("create"));
        assert!(help.contains("list"));
        assert!(help.contains("sync"));
        assert!(help.contains("ready"));
    }

    #[test]
    fn version_includes_name_and_version() {
        let version = Cli::command().render_version();
        assert!(version.contains("br"));
        assert!(version.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn is_mutating_command_detects_mutations() {
        let create_cmd = Commands::Create(make_create_args());
        let list_cmd = Commands::List(beads::cli::ListArgs::default());
        assert!(is_mutating_command(&create_cmd));
        assert!(!is_mutating_command(&list_cmd));
    }

    #[test]
    fn is_mutating_command_distinguishes_read_only_subcommands() {
        let dep_list = Cli::parse_from(["br", "dep", "list", "bd-123"]).command;
        let dep_add = Cli::parse_from(["br", "dep", "add", "bd-123", "bd-456"]).command;
        let label_list = Cli::parse_from(["br", "label", "list"]).command;
        let label_add = Cli::parse_from(["br", "label", "add", "bd-123", "--label", "ops"]).command;
        let comments_list = Cli::parse_from(["br", "comments", "bd-123"]).command;
        let comments_add = Cli::parse_from(["br", "comments", "add", "bd-123", "hello"]).command;
        assert!(!is_mutating_command(&dep_list));
        assert!(is_mutating_command(&dep_add));
        assert!(!is_mutating_command(&label_list));
        assert!(is_mutating_command(&label_add));
        assert!(!is_mutating_command(&comments_list));
        assert!(is_mutating_command(&comments_add));
    }

    #[test]
    fn sync_is_not_auto_imported_or_auto_flushed() {
        let sync_cmd = Cli::parse_from(["br", "sync"]).command;
        assert!(!is_mutating_command(&sync_cmd));
        assert!(!should_auto_import(&sync_cmd));
    }

    #[test]
    fn sync_modes_require_write_lock_before_storage_open() {
        // Regression: `br sync --flush-only` calls `finalize_export` inside a
        // `with_write_transaction` (clears dirty flags, updates
        // jsonl_content_hash + last_export_time + needs_flush metadata, writes
        // export hashes). That makes it a write-side operation as far as
        // the previous storage engine was concerned. Previously the `needs_write_lock` match arm
        // excluded `--flush-only`, leaving two concurrent `br sync
        // --flush-only` invocations — or one racing a mutating command's
        // auto-flush — to hit the previous engine's concurrent-write deadlock that the
        // `.write.lock` was specifically introduced (issue #243) to prevent.
        //
        // `br sync --status` is read-only after storage is open, but the open
        // path can apply runtime metadata defaults, recover from JSONL, or move
        // sidecars. It must therefore serialize before entering `sync::execute`.
        let flush_only = Cli::parse_from(["br", "sync", "--flush-only"]).command;
        let status = Cli::parse_from(["br", "sync", "--status"]).command;
        let witness = Cli::parse_from(["br", "sync", "--witness"]).command;
        let merge = Cli::parse_from(["br", "sync", "--merge"]).command;
        let import_only = Cli::parse_from(["br", "sync", "--import-only"]).command;
        let default_sync = Cli::parse_from(["br", "sync"]).command;

        assert!(
            needs_write_lock(&flush_only),
            "`br sync --flush-only` writes DB metadata and must serialize via .write.lock"
        );
        assert!(
            needs_write_lock(&status),
            "`br sync --status` opens storage and must serialize before recovery/schema work"
        );
        assert!(
            !needs_write_lock(&witness),
            "`br sync --witness` reads JSONL without opening SQLite and should not wait on .write.lock"
        );
        assert!(needs_write_lock(&merge));
        assert!(needs_write_lock(&import_only));
        assert!(
            !needs_write_lock(&default_sync),
            "bare `br sync` fails validation before storage open and should not wait on .write.lock"
        );
    }

    #[test]
    fn diagnostic_and_config_commands_skip_auto_import() {
        let cases: &[&[&str]] = &[&["br", "config", "path"], &["br", "history", "list"]];

        for argv in cases {
            let command = Cli::parse_from(*argv).command;
            assert!(
                !should_auto_import(&command),
                "command should not auto-import: {command:?}"
            );
        }
    }

    #[test]
    fn direct_storage_inspection_commands_require_write_lock() {
        let cases: &[&[&str]] = &[&["br", "list"], &["br", "config", "list"], &["br", "info"]];

        for argv in cases {
            let command = Cli::parse_from(*argv).command;
            assert!(
                needs_write_lock(&command),
                "storage-opening command should serialize DB-family access: {command:?}"
            );
        }
    }

    #[test]
    fn config_path_and_edit_do_not_require_db_write_lock() {
        let cases: &[&[&str]] = &[&["br", "config", "path"], &["br", "config", "edit"]];

        for argv in cases {
            let command = Cli::parse_from(*argv).command;
            assert!(
                !needs_write_lock(&command),
                "config command should not lock when it does not inspect the DB: {command:?}"
            );
        }
    }

    #[test]
    fn auto_import_expected_prefix_uses_merged_config_layers() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        fs::write(
            beads_dir.join("config.yaml"),
            "issue_prefix: document-intelligence\n",
        )
        .expect("write config");

        let mut storage_result =
            config::open_storage_with_cli(&beads_dir, &config::CliOverrides::default())
                .expect("open storage");
        storage_result
            .storage
            .set_config("issue_prefix", "db-prefix")
            .expect("set db prefix");

        let prefix =
            resolve_auto_import_expected_prefix(&storage_result, &config::CliOverrides::default())
                .expect("resolve prefix");

        assert_eq!(prefix, "document-intelligence");
    }

    #[test]
    fn preopened_storage_reuses_startup_paths() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        // `StartupContext::init` resolves the beads dir through
        // `config::discover_beads_dir_with_cli`, which canonicalizes; compare
        // against the same resolved form (macOS /var -> /private/var).
        let resolved_beads_dir = dunce::canonicalize(&beads_dir).expect("canonicalize beads dir");
        let first_jsonl = resolved_beads_dir.join("first.jsonl");
        let second_jsonl = resolved_beads_dir.join("second.jsonl");
        let metadata_path = beads_dir.join("metadata.json");
        fs::write(
            &metadata_path,
            r#"{"database":"beads.db","jsonl_export":"first.jsonl"}"#,
        )
        .expect("write initial metadata");

        let overrides = config::CliOverrides {
            db: Some(beads_dir.join("beads.db")),
            no_db: Some(true),
            ..config::CliOverrides::default()
        };
        let mut ctx = StartupContext::init(&overrides).expect("startup context");

        fs::write(
            &metadata_path,
            r#"{"database":"beads.db","jsonl_export":"second.jsonl"}"#,
        )
        .expect("rewrite metadata");

        let storage_ctx = open_storage_from_ctx(&mut ctx, false).expect("preopened storage");

        assert_eq!(storage_ctx.paths.jsonl_path, first_jsonl);
        assert_ne!(storage_ctx.paths.jsonl_path, second_jsonl);
    }

    #[test]
    fn create_dispatch_reuses_preopened_storage_context() {
        let temp = TempDir::new().expect("tempdir");
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");

        let first_db = beads_dir.join("first.db");
        let second_db = beads_dir.join("second.db");
        let metadata_path = beads_dir.join("metadata.json");
        fs::write(
            &metadata_path,
            format!(
                r#"{{"database":"{}","jsonl_export":"issues.jsonl"}}"#,
                first_db.display()
            ),
        )
        .expect("write initial metadata");

        let overrides = config::CliOverrides::default();
        let startup =
            config::load_startup_config_with_paths(&beads_dir, None).expect("startup context");

        fs::write(
            &metadata_path,
            format!(
                r#"{{"database":"{}","jsonl_export":"issues.jsonl"}}"#,
                second_db.display()
            ),
        )
        .expect("rewrite metadata");

        let cli = Cli::parse_from(["br", "--json", "create", "Use preopened storage"]);
        let output_ctx = OutputContext::from_args(&cli);
        let Commands::Create(args) = cli.command else {
            unreachable!("expected create command");
        };
        let mut storage_result = Some(
            config::open_storage_with_startup_config(startup, &overrides, false)
                .expect("preopened storage"),
        );

        execute_create_command(&args, &overrides, &output_ctx, &mut storage_result)
            .expect("create should use preopened storage");

        assert!(storage_result.is_none());

        let first_storage = beads::storage::SqliteStorage::open(&first_db).expect("open first db");
        assert_eq!(first_storage.count_issues().expect("count first db"), 1);
        assert!(
            !second_db.exists(),
            "create dispatch reopened storage from rewritten metadata instead of using preopened context"
        );
    }

    #[test]
    fn should_render_errors_as_json_when_command_requests_json_format() {
        let cli = Cli::parse_from(["br", "list", "--format", "json"]);
        assert!(should_render_errors_as_json_with_env(&cli, None));
    }

    #[test]
    fn should_render_errors_as_json_when_env_requests_json_format() {
        let cli = Cli::parse_from(["br", "history", "list"]);
        assert!(should_render_errors_as_json_with_env(
            &cli,
            Some(OutputFormat::Json)
        ));
    }

    #[test]
    fn should_not_render_errors_as_json_without_json_request() {
        let cli = Cli::parse_from(["br", "history", "list"]);
        assert!(!should_render_errors_as_json_with_env(&cli, None));
    }

    #[test]
    fn preopen_storage_skips_commands_without_bootstrap_or_flush_work() {
        assert!(!should_preopen_storage(true, false));
    }

    #[test]
    fn preopen_storage_keeps_mutating_auto_flush_path() {
        assert!(should_preopen_storage(true, true));
    }

    #[test]
    fn preopen_storage_keeps_bootstrap_path_for_staleness_checks() {
        assert!(should_preopen_storage(true, true));
    }

    #[test]
    fn preopen_storage_requires_write_lock_before_open() {
        assert!(should_acquire_startup_write_lock(false, true, false));
        assert!(should_acquire_startup_write_lock(true, false, false));
        assert!(should_acquire_startup_write_lock(true, true, false));
        assert!(!should_acquire_startup_write_lock(false, false, false));
        assert!(!should_acquire_startup_write_lock(false, true, true));
        assert!(!should_acquire_startup_write_lock(true, false, true));
        assert!(!should_acquire_startup_write_lock(true, true, true));
    }
}
