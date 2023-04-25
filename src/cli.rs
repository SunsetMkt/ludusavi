mod parse;
mod report;

use clap::CommandFactory;
use indicatif::{ParallelProgressIterator, ProgressBar};
use rayon::{
    iter::{IntoParallelRefIterator, ParallelIterator},
    prelude::IndexedParallelIterator,
};

use crate::{
    cli::{
        parse::{Cli, CompletionShell, ManifestSubcommand, Subcommand},
        report::{report_cloud_changes, Reporter},
    },
    cloud::{CloudChange, Rclone, Remote},
    lang::TRANSLATOR,
    prelude::{app_dir, get_threads_from_env, initialize_rayon, Error, Finality, StrictPath, SyncDirection},
    resource::{cache::Cache, config::Config, manifest::Manifest, ResourceFile, SaveableResourceFile},
    scan::{
        heroic::HeroicGames, layout::BackupLayout, prepare_backup_target, scan_game_for_backup, BackupId,
        DuplicateDetector, InstallDirRanking, OperationStepDecision, SteamShortcuts, TitleFinder,
    },
};

#[derive(Clone, Debug, Default)]
struct GameSubjects {
    valid: Vec<String>,
    invalid: Vec<String>,
}

impl GameSubjects {
    pub fn new(known: Vec<String>, requested: Vec<String>, by_steam_id: bool, manifest: &Manifest) -> Self {
        let mut subjects = Self::default();

        if requested.is_empty() {
            subjects.valid = known;
        } else if by_steam_id {
            let steam_ids_to_names = &manifest.map_steam_ids_to_names();
            for game in requested {
                match game.parse::<u32>() {
                    Ok(id) => {
                        if steam_ids_to_names.contains_key(&id) && known.contains(&steam_ids_to_names[&id]) {
                            subjects.valid.push(steam_ids_to_names[&id].clone());
                        } else {
                            subjects.invalid.push(game);
                        }
                    }
                    Err(_) => {
                        subjects.invalid.push(game);
                    }
                }
            }
        } else {
            for game in requested {
                if known.contains(&game) {
                    subjects.valid.push(game);
                } else {
                    subjects.invalid.push(game);
                }
            }
        }

        subjects.valid.sort();
        subjects.invalid.sort();
        subjects
    }
}

fn warn_deprecations(by_steam_id: bool) {
    if by_steam_id {
        eprintln!("WARNING: `--by-steam-id` is deprecated. Use the `find` command instead.");
    }
}

fn negatable_flag(on: bool, off: bool, default: bool) -> bool {
    if on {
        true
    } else if off {
        false
    } else {
        default
    }
}

pub fn parse() -> Cli {
    use clap::Parser;
    Cli::parse()
}

pub fn run(sub: Subcommand) -> Result<(), Error> {
    let mut config = Config::load()?;
    if let Some(threads) = get_threads_from_env().or(config.runtime.threads) {
        initialize_rayon(threads);
    }
    TRANSLATOR.set_language(config.language);
    let mut cache = Cache::load().unwrap_or_default().migrate_config(&mut config);
    let mut failed = false;
    let mut duplicate_detector = DuplicateDetector::default();

    log::debug!("Config on startup: {config:?}");
    log::debug!("Invocation: {sub:?}");

    match sub {
        Subcommand::Backup {
            preview,
            path,
            force,
            merge,
            no_merge,
            update,
            try_update,
            by_steam_id,
            wine_prefix,
            api,
            sort,
            format,
            compression,
            compression_level,
            full_limit,
            differential_limit,
            cloud_sync,
            no_cloud_sync,
            games,
        } => {
            warn_deprecations(by_steam_id);

            let mut reporter = if api { Reporter::json() } else { Reporter::standard() };

            let manifest = if try_update {
                if let Err(e) = Manifest::update_mut(&config, &mut cache, true) {
                    eprintln!("{}", TRANSLATOR.handle_error(&e));
                }
                Manifest::load().unwrap_or_default()
            } else {
                Manifest::update_mut(&config, &mut cache, update)?;
                Manifest::load()?
            };

            let backup_dir = match path {
                None => config.backup.path.clone(),
                Some(p) => p,
            };
            let roots = config.expanded_roots();

            let merge = negatable_flag(merge, no_merge, config.backup.merge);

            if !preview && !force {
                match dialoguer::Confirm::new()
                    .with_prompt(TRANSLATOR.confirm_backup(&backup_dir, backup_dir.exists(), merge, false))
                    .interact()
                {
                    Ok(true) => (),
                    Ok(false) => return Ok(()),
                    Err(_) => return Err(Error::CliUnableToRequestConfirmation),
                }
            }

            if !preview {
                prepare_backup_target(&backup_dir, merge)?;
            }

            let mut all_games = manifest;
            all_games.incorporate_extensions(&config.roots, &config.custom_games);

            let games_specified = !games.is_empty();
            let subjects = GameSubjects::new(all_games.0.keys().cloned().collect(), games, by_steam_id, &all_games);
            if !subjects.invalid.is_empty() {
                reporter.trip_unknown_games(subjects.invalid.clone());
                reporter.print_failure();
                return Err(Error::CliUnrecognizedGames {
                    games: subjects.invalid,
                });
            }

            let mut retention = config.backup.retention.clone();
            if let Some(full_limit) = full_limit {
                retention.full = full_limit;
            }
            if let Some(differential_limit) = differential_limit {
                retention.differential = differential_limit;
            }

            let layout = BackupLayout::new(backup_dir.clone(), retention);
            let title_finder = TitleFinder::new(&all_games, &layout);
            let heroic_games = HeroicGames::scan(&roots, &title_finder, None);
            let filter = config.backup.filter.clone();
            let ranking = InstallDirRanking::scan(&roots, &all_games, &subjects.valid);
            let toggled_paths = config.backup.toggled_paths.clone();
            let toggled_registry = config.backup.toggled_registry.clone();
            let steam_shortcuts = SteamShortcuts::scan();

            let cloud_sync = negatable_flag(
                cloud_sync,
                no_cloud_sync,
                config.cloud.synchronize && crate::cloud::validate_cloud_config(&config, &config.cloud.path).is_ok(),
            );
            let mut should_sync_cloud_after = cloud_sync;
            if cloud_sync {
                let changes = sync_cloud(
                    &config,
                    &backup_dir,
                    &config.cloud.path,
                    SyncDirection::Download,
                    Finality::Preview,
                    if games_specified { &subjects.valid } else { &[] },
                );
                match changes {
                    Ok(changes) => {
                        if !changes.is_empty() {
                            should_sync_cloud_after = false;
                            reporter.trip_cloud_conflict();
                        }
                    }
                    Err(_) => {
                        should_sync_cloud_after = false;
                        reporter.trip_cloud_sync_failed();
                    }
                }
            }

            log::info!("beginning backup with {} steps", subjects.valid.len());

            let mut info: Vec<_> = subjects
                .valid
                .par_iter()
                .enumerate()
                .progress_with(scan_progress_bar(subjects.valid.len() as u64))
                .map(|(i, name)| {
                    log::trace!("step {i} / {}: {name}", subjects.valid.len());
                    let game = &all_games.0[name];

                    let previous = layout.latest_backup(name, false, &config.redirects);

                    let scan_info = scan_game_for_backup(
                        game,
                        name,
                        &roots,
                        &StrictPath::from_std_path_buf(&app_dir()),
                        &heroic_games,
                        &filter,
                        &wine_prefix,
                        &ranking,
                        &toggled_paths,
                        &toggled_registry,
                        previous,
                        &config.redirects,
                        &steam_shortcuts,
                    );
                    let ignored = !&config.is_game_enabled_for_backup(name) && !games_specified;
                    let decision = if ignored {
                        OperationStepDecision::Ignored
                    } else {
                        OperationStepDecision::Processed
                    };
                    let backup_info = if preview || ignored {
                        crate::scan::BackupInfo::default()
                    } else {
                        let mut backup_format = config.backup.format.clone();
                        if let Some(format) = format {
                            backup_format.chosen = format;
                        }
                        if let Some(compression) = compression {
                            backup_format.zip.compression = compression;
                        }
                        if let Some(level) = compression_level {
                            backup_format
                                .compression
                                .set_level(&backup_format.zip.compression, level);
                        }

                        layout
                            .game_layout(name)
                            .back_up(&scan_info, merge, &chrono::Utc::now(), &backup_format)
                    };
                    log::trace!("step {i} completed");
                    (name, scan_info, backup_info, decision)
                })
                .collect();
            log::info!("completed backup");

            if should_sync_cloud_after {
                let sync_result = sync_cloud(
                    &config,
                    &backup_dir,
                    &config.cloud.path,
                    SyncDirection::Upload,
                    Finality::Final,
                    if games_specified { &subjects.valid } else { &[] },
                );
                if sync_result.is_err() {
                    reporter.trip_cloud_sync_failed();
                }
            }

            for (_, scan_info, _, _) in info.iter() {
                if !scan_info.can_report_game() {
                    continue;
                }
                duplicate_detector.add_game(
                    scan_info,
                    config.is_game_enabled_for_operation(&scan_info.game_name, false),
                );
            }

            let sort = sort.map(From::from).unwrap_or_else(|| config.backup.sort.clone());
            info.sort_by(|(_, scan_info1, backup_info1, ..), (_, scan_info2, backup_info2, ..)| {
                crate::scan::compare_games(sort.key, scan_info1, Some(backup_info1), scan_info2, Some(backup_info2))
            });
            if sort.reversed {
                info.reverse();
            }

            for (name, scan_info, backup_info, decision) in info {
                if !reporter.add_game(name, &scan_info, &backup_info, &decision, &duplicate_detector) {
                    failed = true;
                }
            }
            reporter.print(&backup_dir);
        }
        Subcommand::Restore {
            preview,
            path,
            force,
            by_steam_id,
            api,
            sort,
            backup,
            cloud_sync,
            no_cloud_sync,
            games,
        } => {
            warn_deprecations(by_steam_id);

            let mut reporter = if api { Reporter::json() } else { Reporter::standard() };

            if !Manifest::path().exists() {
                Manifest::update_mut(&config, &mut cache, true)?;
            }
            let manifest = Manifest::load()?;

            let restore_dir = match path {
                None => config.restore.path.clone(),
                Some(p) => p,
            };

            if !preview && !force {
                match dialoguer::Confirm::new()
                    .with_prompt(TRANSLATOR.confirm_restore(&restore_dir, false))
                    .interact()
                {
                    Ok(true) => (),
                    Ok(false) => return Ok(()),
                    Err(_) => return Err(Error::CliUnableToRequestConfirmation),
                }
            }

            let layout = BackupLayout::new(restore_dir.clone(), config.backup.retention.clone());

            let restorable_names = layout.restorable_games();

            if backup.is_some() && games.len() != 1 {
                return Err(Error::CliBackupIdWithMultipleGames);
            }
            let backup_id = backup.as_ref().map(|x| BackupId::Named(x.clone()));

            let games_specified = !games.is_empty();
            let subjects = GameSubjects::new(restorable_names, games, by_steam_id, &manifest);
            if !subjects.invalid.is_empty() {
                reporter.trip_unknown_games(subjects.invalid.clone());
                reporter.print_failure();
                return Err(Error::CliUnrecognizedGames {
                    games: subjects.invalid,
                });
            }

            let cloud_sync = negatable_flag(
                cloud_sync,
                no_cloud_sync,
                config.cloud.synchronize && crate::cloud::validate_cloud_config(&config, &config.cloud.path).is_ok(),
            );
            if cloud_sync {
                let changes = sync_cloud(
                    &config,
                    &restore_dir,
                    &config.cloud.path,
                    SyncDirection::Download,
                    Finality::Preview,
                    if games_specified { &subjects.valid } else { &[] },
                );
                match changes {
                    Ok(changes) => {
                        if !changes.is_empty() {
                            reporter.trip_cloud_conflict();
                        }
                    }
                    Err(_) => {
                        reporter.trip_cloud_sync_failed();
                    }
                }
            }

            log::info!("beginning restore with {} steps", subjects.valid.len());

            let mut info: Vec<_> = subjects
                .valid
                .par_iter()
                .enumerate()
                .progress_with(scan_progress_bar(subjects.valid.len() as u64))
                .map(|(i, name)| {
                    log::trace!("step {i} / {}: {name}", subjects.valid.len());
                    let mut layout = layout.game_layout(name);
                    let scan_info = layout.scan_for_restoration(
                        name,
                        backup_id.as_ref().unwrap_or(&BackupId::Latest),
                        &config.redirects,
                    );
                    let ignored = !&config.is_game_enabled_for_restore(name) && !games_specified;
                    let decision = if ignored {
                        OperationStepDecision::Ignored
                    } else {
                        OperationStepDecision::Processed
                    };

                    if let Some(backup) = &backup {
                        if let Some(BackupId::Named(scanned_backup)) = scan_info.backup.as_ref().map(|x| x.id()) {
                            if backup != &scanned_backup {
                                log::trace!("step {i} completed (backup mismatch)");
                                return (
                                    name,
                                    scan_info,
                                    Default::default(),
                                    decision,
                                    Some(Err(Error::CliInvalidBackupId)),
                                );
                            }
                        }
                    }

                    let restore_info = if scan_info.backup.is_none() || preview || ignored {
                        crate::scan::BackupInfo::default()
                    } else {
                        layout.restore(&scan_info)
                    };
                    log::trace!("step {i} completed");
                    (name, scan_info, restore_info, decision, None)
                })
                .collect();
            log::info!("completed restore");

            for (_, scan_info, _, _, failure) in info.iter() {
                if !scan_info.can_report_game() {
                    continue;
                }
                if let Some(failure) = failure {
                    return failure.clone();
                }
                duplicate_detector.add_game(
                    scan_info,
                    config.is_game_enabled_for_operation(&scan_info.game_name, true),
                );
            }

            let sort = sort.map(From::from).unwrap_or_else(|| config.restore.sort.clone());
            info.sort_by(|(_, scan_info1, backup_info1, ..), (_, scan_info2, backup_info2, ..)| {
                crate::scan::compare_games(sort.key, scan_info1, Some(backup_info1), scan_info2, Some(backup_info2))
            });
            if sort.reversed {
                info.reverse();
            }

            for (name, scan_info, backup_info, decision, _) in info {
                if !reporter.add_game(name, &scan_info, &backup_info, &decision, &duplicate_detector) {
                    failed = true;
                }
            }
            reporter.print(&restore_dir);
        }
        Subcommand::Complete { shell } => {
            let clap_shell = match shell {
                CompletionShell::Bash => clap_complete::Shell::Bash,
                CompletionShell::Fish => clap_complete::Shell::Fish,
                CompletionShell::Zsh => clap_complete::Shell::Zsh,
                CompletionShell::PowerShell => clap_complete::Shell::PowerShell,
                CompletionShell::Elvish => clap_complete::Shell::Elvish,
            };
            clap_complete::generate(
                clap_shell,
                &mut Cli::command(),
                env!("CARGO_PKG_NAME"),
                &mut std::io::stdout(),
            )
        }
        Subcommand::Backups {
            path,
            by_steam_id,
            api,
            games,
        } => {
            warn_deprecations(by_steam_id);

            let mut reporter = if api { Reporter::json() } else { Reporter::standard() };
            reporter.suppress_overall();

            if !Manifest::path().exists() {
                Manifest::update_mut(&config, &mut cache, true)?;
            }
            let manifest = Manifest::load()?;

            let restore_dir = match path {
                None => config.restore.path.clone(),
                Some(p) => p,
            };

            let layout = BackupLayout::new(restore_dir.clone(), config.backup.retention.clone());

            let restorable_names = layout.restorable_games();

            let subjects = GameSubjects::new(restorable_names, games, by_steam_id, &manifest);
            if !subjects.invalid.is_empty() {
                reporter.trip_unknown_games(subjects.invalid.clone());
                reporter.print_failure();
                return Err(Error::CliUnrecognizedGames {
                    games: subjects.invalid,
                });
            }

            let info: Vec<_> = subjects
                .valid
                .par_iter()
                .progress_count(subjects.valid.len() as u64)
                .map(|name| {
                    let mut layout = layout.game_layout(name);
                    let backups = layout.get_backups();
                    (name, backups)
                })
                .collect();

            for (name, backups) in info {
                reporter.add_backups(name, &backups);
            }
            reporter.print(&restore_dir);
        }
        Subcommand::Find {
            api,
            path,
            backup,
            restore,
            steam_id,
            gog_id,
            normalized,
            names,
        } => {
            let mut reporter = if api { Reporter::json() } else { Reporter::standard() };
            reporter.suppress_overall();

            if let Err(e) = Manifest::update_mut(&config, &mut cache, false) {
                eprintln!("{}", TRANSLATOR.handle_error(&e));
            }
            let mut manifest = Manifest::load().unwrap_or_default();
            manifest.incorporate_extensions(&config.roots, &config.custom_games);

            let restore_dir = match path {
                None => config.restore.path.clone(),
                Some(p) => p,
            };
            let layout = BackupLayout::new(restore_dir.clone(), config.backup.retention.clone());

            let title_finder = TitleFinder::new(&manifest, &layout);
            let found = title_finder.find(&names, &steam_id, &gog_id, normalized, backup, restore);
            reporter.add_found_titles(&found);

            if found.is_empty() {
                let mut invalid = names;
                if let Some(steam_id) = steam_id {
                    invalid.push(steam_id.to_string());
                }
                if let Some(gog_id) = gog_id {
                    invalid.push(gog_id.to_string());
                }
                reporter.trip_unknown_games(invalid.clone());
                reporter.print_failure();
                return Err(Error::CliUnrecognizedGames { games: invalid });
            }

            reporter.print(&restore_dir);
        }
        Subcommand::Manifest { sub: manifest_sub } => {
            if let Some(ManifestSubcommand::Show { api }) = manifest_sub {
                let mut manifest = Manifest::load().unwrap_or_default();
                manifest.incorporate_extensions(&config.roots, &config.custom_games);

                if api {
                    println!("{}", serde_json::to_string(&manifest).unwrap());
                } else {
                    println!("{}", serde_yaml::to_string(&manifest).unwrap());
                }
            }
        }
        Subcommand::Cloud { sub: cloud_sub } => match cloud_sub {
            parse::CloudSubcommand::Set { sub } => match sub {
                parse::CloudSetSubcommand::None => {
                    config.cloud.remote = None;
                    config.save();
                }
                parse::CloudSetSubcommand::Custom { name } => {
                    configure_cloud(&mut config, Remote::Custom { name })?;
                }
                parse::CloudSetSubcommand::Box => {
                    configure_cloud(&mut config, Remote::Box)?;
                }
                parse::CloudSetSubcommand::Dropbox => {
                    configure_cloud(&mut config, Remote::Dropbox)?;
                }
                parse::CloudSetSubcommand::Ftp {
                    host,
                    port,
                    username,
                    password,
                } => {
                    configure_cloud(
                        &mut config,
                        Remote::Ftp {
                            host,
                            port,
                            username,
                            password,
                        },
                    )?;
                }
                parse::CloudSetSubcommand::GoogleDrive => {
                    configure_cloud(&mut config, Remote::GoogleDrive)?;
                }
                parse::CloudSetSubcommand::OneDrive => {
                    configure_cloud(&mut config, Remote::OneDrive)?;
                }
                parse::CloudSetSubcommand::Smb {
                    host,
                    port,
                    username,
                    password,
                } => {
                    configure_cloud(
                        &mut config,
                        Remote::Smb {
                            host,
                            port,
                            username,
                            password,
                        },
                    )?;
                }
                parse::CloudSetSubcommand::WebDav {
                    url,
                    username,
                    password,
                    provider,
                } => {
                    configure_cloud(
                        &mut config,
                        Remote::WebDav {
                            url,
                            username,
                            password,
                            provider,
                        },
                    )?;
                }
            },
            parse::CloudSubcommand::Upload {
                local,
                cloud,
                force,
                preview,
                games,
            } => {
                let local = local.unwrap_or(config.backup.path.clone());
                let cloud = cloud.unwrap_or(config.cloud.path.clone());

                let finality = if preview { Finality::Preview } else { Finality::Final };
                let direction = SyncDirection::Upload;

                if !ask(
                    TRANSLATOR.confirm_cloud_upload(&local.render(), &cloud),
                    finality,
                    force,
                )? {
                    return Ok(());
                }

                let changes = sync_cloud(&config, &local, &cloud, direction, finality, &games)?;
                report_cloud_changes(&changes);
            }
            parse::CloudSubcommand::Download {
                local,
                cloud,
                force,
                preview,
                games,
            } => {
                let local = local.unwrap_or(config.backup.path.clone());
                let cloud = cloud.unwrap_or(config.cloud.path.clone());

                let finality = if preview { Finality::Preview } else { Finality::Final };
                let direction = SyncDirection::Download;

                if !ask(
                    TRANSLATOR.confirm_cloud_download(&local.render(), &cloud),
                    finality,
                    force,
                )? {
                    return Ok(());
                }

                let changes = sync_cloud(&config, &local, &cloud, direction, finality, &games)?;
                report_cloud_changes(&changes);
            }
        },
    }

    if failed {
        Err(Error::SomeEntriesFailed)
    } else {
        Ok(())
    }
}

fn configure_cloud(config: &mut Config, remote: Remote) -> Result<(), Error> {
    if remote.needs_configuration() {
        let rclone = Rclone::new(config.apps.rclone.clone(), remote.clone());
        rclone.configure_remote().map_err(Error::UnableToConfigureCloud)?;
    }
    config.cloud.remote = Some(remote);
    config.save();
    Ok(())
}

fn ask(question: String, finality: Finality, force: bool) -> Result<bool, Error> {
    if finality.preview() || force {
        Ok(true)
    } else {
        dialoguer::Confirm::new()
            .with_prompt(question)
            .interact()
            .map_err(|_| Error::CliUnableToRequestConfirmation)
    }
}

fn scan_progress_bar(length: u64) -> ProgressBar {
    let template = format!(
        "{} ({{elapsed_precise}}) {{wide_bar}} {{pos}} / {{len}} {}",
        TRANSLATOR.scan_label(),
        TRANSLATOR.games_unit()
    );
    let style = indicatif::ProgressStyle::default_bar().template(&template);
    ProgressBar::new(length).with_style(style)
}

fn cloud_progress_bar() -> ProgressBar {
    let template = format!(
        "{} ({{elapsed_precise}}) {{wide_bar}} {{msg}}",
        TRANSLATOR.cloud_label()
    );
    let style = indicatif::ProgressStyle::default_bar().template(&template);
    ProgressBar::new(100).with_style(style)
}

fn sync_cloud(
    config: &Config,
    local: &StrictPath,
    cloud: &str,
    sync: SyncDirection,
    finality: Finality,
    games: &[String],
) -> Result<Vec<CloudChange>, Error> {
    match finality {
        Finality::Preview => log::info!("checking cloud sync"),
        Finality::Final => log::info!("performing cloud sync"),
    }

    let remote = crate::cloud::validate_cloud_config(config, cloud)?;

    let layout = BackupLayout::new(local.clone(), config.backup.retention.clone());
    let games: Vec<_> = games.iter().filter_map(|x| layout.game_folder(x).leaf()).collect();

    let rclone = Rclone::new(config.apps.rclone.clone(), remote);
    let mut process = match rclone.sync(local, cloud, sync, finality, &games) {
        Ok(p) => p,
        Err(e) => return Err(Error::UnableToSynchronizeCloud(e)),
    };

    let progress_bar = cloud_progress_bar();
    let mut changes = vec![];
    loop {
        let events = process.events();
        for event in events {
            match event {
                crate::cloud::RcloneProcessEvent::Progress { current, max } => {
                    progress_bar.set_length(max as u64);
                    progress_bar.set_position(current as u64);
                    progress_bar.set_message(TRANSLATOR.cloud_progress(current as u64, max as u64))
                }
                crate::cloud::RcloneProcessEvent::Change(change) => {
                    changes.push(change);
                }
            }
        }
        match process.succeeded() {
            Some(Ok(_)) => return Ok(changes),
            Some(Err(e)) => {
                progress_bar.finish_and_clear();
                return Err(Error::UnableToSynchronizeCloud(e));
            }
            None => (),
        }
    }
}
