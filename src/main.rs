//! Refrain — a fast, low-memory desktop client for YouTube Music.

// A GUI app on Windows should not open a console beside itself. Debug builds
// keep it, because that is where the diagnostics go.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod auth;
mod config;
mod media;
mod thumbs;
mod ui;
mod update;
mod ytm;

/// Every option the dispatcher above knows about, for the typo warning.
const KNOWN_FLAGS: [&str; 19] = [
    "--auth-check",
    "--canary",
    "--decode",
    "--demo",
    "--download",
    "--fps",
    "--help",
    "--like-status",
    "--page",
    "--play-local",
    "--play-test",
    "--probe",
    "--refresh",
    "--search",
    "--stream",
    "--thumb",
    "--version",
    "-V",
    "-h",
];

const USAGE: &str = "Refrain — a desktop client for YouTube Music.

  refrain                      open the app
  refrain --demo               run the scripted demo
  refrain --fps                print the frame rate the platform delivers
  refrain --canary             check yt-dlp, the JavaScript runtime and the API
  refrain --play-test <id>     play a track muted, and assert that it plays
  refrain --stream <id>        stream a track and report time to first audio
  refrain --play-local <path>  play a local file muted (device versus stream)
  refrain --auth-check         report where the session comes from
  refrain --search <query>     print parsed search results in order
  refrain --thumb <url>        fetch and decode artwork
  refrain --page <id>          dump a detail page's tracks and shelves
  refrain --like-status <id>   read the rating YouTube holds for a track
  refrain --decode <path>      check that rodio can decode an audio file
  refrain --probe <query>      exercise the InnerTube client without a window
  refrain --version            print the version
  refrain --help               this list
";

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // `--help` lists the diagnostics. There are enough of them now that
    // guessing is unreasonable, and a mistyped one used to open the window
    // instead of saying so.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
    }

    // `--canary` checks the things that break silently, for CI to run on a
    // schedule: see the workflow for why.
    if args.iter().any(|a| a == "--canary") {
        if let Err(e) = canary_cmd() {
            eprintln!("\ncanary failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--version` so a bug report can say which build it is.
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("refrain {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if args.iter().any(|a| a == "--auth-check") {
        if args.iter().any(|a| a == "--refresh") {
            auth::clear_cache();
            println!("cleared cached cookies (they will be re-extracted)\n");
        }
        if let Err(e) = auth_check() {
            eprintln!("auth-check failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--thumb <url>` verifies the artwork pipeline (fetch + decode).
    if let Some(pos) = args.iter().position(|a| a == "--thumb") {
        let url = args.get(pos + 1).cloned().unwrap_or_default();
        if url.is_empty() {
            eprintln!("usage: --thumb <url>");
            std::process::exit(2);
        }
        if let Err(e) = thumb_cmd(&url) {
            eprintln!("thumb failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--search <query>` prints parsed search results in order.
    if let Some(pos) = args.iter().position(|a| a == "--search") {
        let query = args.get(pos + 1).cloned().unwrap_or_default();
        if query.is_empty() {
            eprintln!("usage: --search <query>");
            std::process::exit(2);
        }
        if let Err(e) = search_cmd(&query) {
            eprintln!("search failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--play-local <path>` plays a local file muted (isolates device vs stream).
    if let Some(pos) = args.iter().position(|a| a == "--play-local") {
        let path = args.get(pos + 1).cloned().unwrap_or_default();
        if path.is_empty() {
            eprintln!("usage: --play-local <path>");
            std::process::exit(2);
        }
        if let Err(e) = play_local_cmd(&path) {
            eprintln!("play-local failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--play-test <video_id>` exercises the exact in-app playback path, muted.
    if let Some(pos) = args.iter().position(|a| a == "--play-test") {
        let id = args.get(pos + 1).cloned().unwrap_or_default();
        if id.is_empty() {
            eprintln!("usage: --play-test <video_id>");
            std::process::exit(2);
        }
        if let Err(e) = play_test_cmd(&id) {
            eprintln!("play-test failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--stream <video_id>` verifies streaming and reports time-to-first-audio.
    if let Some(pos) = args.iter().position(|a| a == "--stream") {
        let id = args.get(pos + 1).cloned().unwrap_or_default();
        if id.is_empty() {
            eprintln!("usage: --stream <video_id>");
            std::process::exit(2);
        }
        if let Err(e) = stream_cmd(&id) {
            eprintln!("stream failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--like-status <video_id>` reads the rating YouTube holds for a track.
    // Read-only on purpose: it must never change the listener's library.
    if let Some(pos) = args.iter().position(|a| a == "--like-status") {
        let id = args.get(pos + 1).cloned().unwrap_or_default();
        if id.is_empty() {
            eprintln!("usage: --like-status <video_id>");
            std::process::exit(2);
        }
        if let Err(e) = like_status_cmd(&id) {
            eprintln!("like-status failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--decode <path>` checks that rodio can decode a given audio file.
    if let Some(pos) = args.iter().position(|a| a == "--decode") {
        let path = args.get(pos + 1).cloned().unwrap_or_default();
        if path.is_empty() {
            eprintln!("usage: --decode <path>");
            std::process::exit(2);
        }
        if let Err(e) = decode_cmd(&path) {
            eprintln!("decode failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--page <browse_id>` dumps a detail page's tracks and shelves.
    if let Some(pos) = args.iter().position(|a| a == "--page") {
        let id = args.get(pos + 1).cloned().unwrap_or_default();
        if id.is_empty() {
            eprintln!("usage: --page <browse_id>");
            std::process::exit(2);
        }
        if let Err(e) = page_cmd(&id) {
            eprintln!("page failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `--probe <query>` exercises the InnerTube client without opening a window —
    // handy for checking that the (unofficial) API still responds as expected.
    if let Some(pos) = args.iter().position(|a| a == "--probe") {
        let query = args
            .get(pos + 1)
            .cloned()
            .unwrap_or_else(|| "daft punk".to_string());
        if let Err(e) = probe(&query) {
            eprintln!("probe failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    if let Some(pos) = args.iter().position(|a| a == "--download") {
        let id = args.get(pos + 1).cloned().unwrap_or_default();
        if id.is_empty() {
            eprintln!("usage: --download <video_id>");
            std::process::exit(2);
        }
        if let Err(e) = download_cmd(&id) {
            eprintln!("download failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // eframe sets this at runtime on Windows and macOS, and ignores it on
    // Linux — where the icon comes from the desktop file instead, matched to
    // the window by StartupWMClass. So this is not dead code on Linux, it is
    // simply somebody else's job there.
    let icon = image::load_from_memory(include_bytes!("../packaging/refrain-256.png"))
        .expect("the app icon is embedded in the binary")
        .to_rgba8();
    let (width, height) = icon.dimensions();

    // Anything left that looks like a flag is a typo, and opening a window in
    // silence is a poor way to say so. Only a warning: the bare command is the
    // app, and someone who typed a flag they half-remembered probably wants it.
    for arg in args.iter().skip(1).filter(|arg| arg.starts_with("--")) {
        if !KNOWN_FLAGS.contains(&arg.as_str()) {
            eprintln!("unknown option {arg} — try --help");
        }
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([820.0, 520.0])
            .with_title("Refrain")
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            }),
        ..Default::default()
    };

    eframe::run_native(
        "Refrain",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)?))),
    )
}

/// Diagnose authentication without opening a window.
fn auth_check() -> anyhow::Result<()> {
    let cfg = config::Config::load();
    println!("config file : {}", config::Config::path().display());
    println!("auth mode   : {:?}", cfg.auth.mode);
    println!("yt-dlp      : {:?}", audio::resolver::locate_ytdlp());
    println!("detected cookie sources:");
    let sources = auth::detected_sources();
    if sources.is_empty() {
        println!("  (none found)");
    }
    for s in &sources {
        println!("  - {s}");
    }

    let (lang, country) = config::Config::locale();
    let rt = tokio::runtime::Runtime::new()?;
    let Some(resolved) = rt.block_on(auth::resolve(&cfg))? else {
        println!("\nNo cookie source configured/auto-detected — running anonymously.");
        return Ok(());
    };

    println!(
        "\nchosen source: {} {}",
        resolved.source.browser, resolved.source.profile
    );
    println!(
        "cookies     : {} total | SAPISID: {} | YouTube session (LOGIN_INFO): {}",
        resolved.jar.cookies.len(),
        resolved.jar.has_sapisid(),
        resolved.jar.has_youtube_session()
    );

    let client = ytm::Client::new(&lang, &country, Some(resolved.jar))?;
    println!("client signed: {}", client.is_authenticated());
    match rt.block_on(client.logged_in()) {
        Ok(true) => println!("InnerTube   : LOGGED IN ✅  (library should load)"),
        Ok(false) => println!(
            "InnerTube   : anonymous ❌  — sign into music.youtube.com in your browser, then re-run"
        ),
        Err(e) => println!("InnerTube   : request failed: {e}"),
    }
    Ok(())
}

fn search_cmd(query: &str) -> anyhow::Result<()> {
    use ytm::SearchFilter;

    let (lang, country) = config::Config::locale();
    let cfg = config::Config::load();
    let rt = tokio::runtime::Runtime::new()?;
    let jar = rt
        .block_on(auth::resolve(&cfg))
        .ok()
        .flatten()
        .map(|r| r.jar);
    let client = ytm::Client::new(&lang, &country, jar)?;

    let items = rt.block_on(client.search(query, SearchFilter::All))?.0;
    println!("{} items (All filter)", items.len());
    for (i, item) in items.iter().enumerate() {
        println!(
            "  {i:2}  {:<8}  {:<40}  {:<28}  {}",
            item.kind.label(),
            item.title,
            item.subtitle,
            item.browse_id
                .clone()
                .or_else(|| item.video_id.clone())
                .unwrap_or_else(|| "-".to_string())
        );
    }
    Ok(())
}

fn play_local_cmd(path: &str) -> anyhow::Result<()> {
    let mut engine = audio::AudioEngine::new()?;
    engine.set_volume(0.0);
    let track = ytm::Track {
        video_id: "local".to_string(),
        title: "local".to_string(),
        ..Default::default()
    };
    let t = std::time::Instant::now();
    engine.play_file(std::path::Path::new(path), track)?;
    println!("play_file returned in {:?}", t.elapsed());
    std::thread::sleep(std::time::Duration::from_secs(3));
    println!(
        "after 3s: playing={} pos={:?} total={:?}",
        engine.is_playing(),
        engine.position(),
        engine.total
    );
    // A playback check that works anywhere, with no display: this is what says
    // the audio path is alive on a machine the app has never run on.
    if !engine.is_playing() {
        anyhow::bail!(
            "three seconds in, playback does not report as playing (position {:?})",
            engine.position()
        );
    }
    engine.stop();
    Ok(())
}

/// Exercise the app's real playback path (AudioEngine + RemoteSource), muted so
/// it doesn't make noise.
fn play_test_cmd(video_id: &str) -> anyhow::Result<()> {
    use rodio::Source as _;

    let cfg = config::Config::load();
    let rt = tokio::runtime::Runtime::new()?;
    let auth = rt
        .block_on(auth::resolve(&cfg))?
        .map(|resolved| resolved.source)
        .unwrap_or_default();

    let mut engine = audio::AudioEngine::new()?;
    engine.set_volume(0.0);

    let args = audio::resolver::stream_args(video_id, &auth);
    let cache = Some(audio::resolver::cache_path(video_id));

    // Phase 1: what hover/page-load prefetching does (hidden from the user).
    let prefetch = std::time::Instant::now();
    let source = audio::stream::RemoteSource::new(args, cache)?;
    let failure = source.failure_handle();
    let decoder = rodio::Decoder::new(source)
        .map_err(|e| anyhow::anyhow!(failure.get().unwrap_or_else(|| e.to_string())))?;
    println!(
        "prefetch (open + decode) took {:?} (total {:?})",
        prefetch.elapsed(),
        decoder.total_duration()
    );

    let track = ytm::Track {
        video_id: video_id.to_string(),
        title: "play-test".to_string(),
        ..Default::default()
    };

    // Phase 2: the user clicks — this is now nearly free.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let clicked = std::time::Instant::now();
    engine.play_decoder(decoder, track)?;
    println!("click -> playing in {:?}", clicked.elapsed());

    std::thread::sleep(std::time::Duration::from_secs(3));
    println!(
        "after 3s: playing={} pos={:?} total={:?} finished={}",
        engine.is_playing(),
        engine.position(),
        engine.total,
        engine.finished()
    );
    engine.stop();
    Ok(())
}

fn stream_cmd(video_id: &str) -> anyhow::Result<()> {
    use rodio::Source as _;

    let cfg = config::Config::load();
    let rt = tokio::runtime::Runtime::new()?;
    let auth = rt
        .block_on(auth::resolve(&cfg))?
        .map(|resolved| resolved.source)
        .unwrap_or_default();

    let args = audio::resolver::stream_args(video_id, &auth);
    let cache = Some(audio::resolver::cache_path(video_id));

    let started = std::time::Instant::now();
    let source = audio::stream::RemoteSource::new(args, cache)?;
    let failure = source.failure_handle();
    let decoder = rodio::Decoder::new(source)
        .map_err(|e| anyhow::anyhow!(failure.get().unwrap_or_else(|| e.to_string())))?;
    println!(
        "decoder opened in {:?} (total {:?})",
        started.elapsed(),
        decoder.total_duration()
    );

    let first = std::time::Instant::now();
    let mut decoder = decoder;
    let samples = decoder.by_ref().take(44_100 * 2).count(); // ~1s stereo
    println!(
        "first ~1s of audio decoded after {:?} ({} samples)",
        first.elapsed(),
        samples
    );
    Ok(())
}

fn decode_cmd(path: &str) -> anyhow::Result<()> {
    use rodio::Source as _;
    use std::io::BufReader;

    let file = std::fs::File::open(path)?;
    let mut decoder = rodio::Decoder::new(BufReader::new(file))?;
    let duration = decoder.total_duration();
    let samples = decoder.by_ref().take(65_536).count();
    println!(
        "decode OK: duration={:?}, read {} samples",
        duration, samples
    );
    Ok(())
}

fn thumb_cmd(url: &str) -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let client = reqwest::Client::builder()
        .user_agent(thumbs::USER_AGENT)
        .build()?;
    let bytes = rt.block_on(thumbs::fetch(&client, url))?;
    match thumbs::decode(&bytes, thumbs::MAX_EDGE) {
        Some(image) => {
            println!(
                "fetched {} bytes -> decoded {}x{} ({} px)",
                bytes.len(),
                image.size[0],
                image.size[1],
                image.pixels.len()
            );
            Ok(())
        }
        None => anyhow::bail!("could not decode {} bytes", bytes.len()),
    }
}

fn page_cmd(browse_id: &str) -> anyhow::Result<()> {
    let (lang, country) = config::Config::locale();
    let rt = tokio::runtime::Runtime::new()?;
    let cfg = config::Config::load();
    let jar = rt
        .block_on(auth::resolve(&cfg))
        .ok()
        .flatten()
        .map(|r| r.jar);
    let client = ytm::Client::new(&lang, &country, jar)?;
    let mut page = rt.block_on(client.browse(browse_id))?;

    println!("{:?} | {} | {}", page.kind, page.title, page.subtitle);
    println!("tracks: {}", page.tracks.len());
    println!("track count (header): {:?}", page.track_count);
    println!("has more tracks: {}", page.has_more_tracks());
    println!("owner is a user: {}", page.owner_is_user);
    for t in page.tracks.iter().take(3) {
        println!("   - {} — {}", t.title, t.artist_line());
    }
    println!("shelves: {}", page.shelves.len());
    // Feeds page in more shelves; mirror that so the dump is representative.
    let mut token = page.continuation.clone();
    let mut round = 0;
    while let Some(t) = token.take() {
        if round >= 5 {
            break;
        }
        round += 1;
        match rt.block_on(client.browse_continuation(&t)) {
            Ok(more) => {
                for shelf in &more.shelves {
                    println!("   [{}] {} items", shelf.title, shelf.items.len());
                }
                token = more.continuation;
            }
            Err(e) => {
                println!("   (continuation failed: {e})");
                break;
            }
        }
    }

    // Long playlists cap their first response at 100 tracks; page the rest in
    // through the player panel, exactly as the app's "Load more" does.
    if page.has_more_tracks()
        && let Some(playlist_id) = page.playlist_id.clone()
    {
        for round in 0..10 {
            let Some(anchor) = page.tracks.last().map(|track| track.video_id.clone()) else {
                break;
            };
            match rt.block_on(client.playlist_more(&anchor, &playlist_id)) {
                Ok(tracks) => {
                    let known: std::collections::HashSet<String> =
                        page.tracks.iter().map(|t| t.video_id.clone()).collect();
                    let window = tracks.len();
                    let fresh: Vec<_> = tracks
                        .into_iter()
                        .filter(|track| !known.contains(&track.video_id))
                        .collect();
                    println!(
                        "   (panel round {}: anchor={} window={} fresh={})",
                        round + 1,
                        &anchor[..6.min(anchor.len())],
                        window,
                        fresh.len()
                    );
                    if fresh.is_empty() {
                        break;
                    }
                    page.tracks.extend(fresh);
                    if !page.has_more_tracks() {
                        break;
                    }
                }
                Err(e) => {
                    println!("   (panel failed: {e})");
                    break;
                }
            }
        }
    }

    for shelf in &page.shelves {
        println!("   [{}] {} items", shelf.title, shelf.items.len());
        for item in shelf.items.iter().take(3) {
            println!(
                "       {:?}  {}  —  {}  [{}]",
                item.kind,
                item.title,
                item.subtitle,
                item.browse_id.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}

fn download_cmd(video_id: &str) -> anyhow::Result<()> {
    use rodio::Source as _;
    use std::io::BufReader;

    let cfg = config::Config::load();
    let rt = tokio::runtime::Runtime::new()?;
    // Resolve the effective cookie source first (auto-detection included), so
    // playback uses the same browser that authenticated the catalogue.
    let auth = rt
        .block_on(auth::resolve(&cfg))?
        .map(|resolved| resolved.source)
        .unwrap_or_default();
    // Resolve-and-fetch beats yt-dlp's own downloader once the URL is known,
    // so try that first and keep yt-dlp as the fallback.
    let started = std::time::Instant::now();
    let url = rt.block_on(audio::resolver::resolve_url(video_id, &auth))?;
    println!("resolved in {:.2}s", started.elapsed().as_secs_f32());
    let fetch_started = std::time::Instant::now();
    let path = rt.block_on(audio::resolver::fetch_audio(&url, video_id))?;
    println!("fetched in {:.2}s", fetch_started.elapsed().as_secs_f32());
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("cached {} ({} bytes)", path.display(), size);

    // Prove rodio can actually decode what yt-dlp produced.
    let file = std::fs::File::open(&path)?;
    let mut decoder = rodio::Decoder::new(BufReader::new(file))?;
    let duration = decoder.total_duration();
    let samples = decoder.by_ref().take(4096).count();
    println!(
        "rodio decode OK: duration={:?}, first {} samples read",
        duration, samples
    );
    Ok(())
}

/// Check the parts that would otherwise break silently.
///
/// Our parsing depends on YouTube's response shapes, which change without
/// notice and fail *quietly* — an empty list looks like "no results", not like
/// a bug. This asserts on the live API instead, and on yt-dlp still working.
///
/// It runs **anonymously**, because CI has no browser session, which means it
/// exercises the paths that work without one. Targets are found by searching
/// rather than hard-coded, so the check doesn't rot as videos and playlists get
/// removed.
fn canary_cmd() -> anyhow::Result<()> {
    use config::AuthMode;
    use ytm::{Client, SearchFilter};

    let (lang, country) = config::Config::locale();
    let rt = tokio::runtime::Runtime::new()?;
    let client = Client::new(&lang, &country, None)?;
    let mut failures: Vec<String> = Vec::new();

    // 1. Search — and it hands us a real video to test yt-dlp with below.
    let songs = match rt.block_on(client.search("daft punk", SearchFilter::Songs)) {
        Ok((items, more)) => {
            println!(
                "ok      search: {} songs, next page: {}",
                items.len(),
                more.is_some()
            );
            items
        }
        Err(e) => {
            failures.push(format!("search failed: {e}"));
            Vec::new()
        }
    };

    // 2. A detail page, found by searching rather than hard-coded.
    match songs.iter().find_map(|item| item.video_id.clone()) {
        Some(video_id) => {
            println!("ok      search returned a video id: {video_id}");

            // 3. yt-dlp must be present, run, and still resolve a stream. A bot
            //    check is YouTube answering rather than yt-dlp breaking, so it
            //    is a note; anything else means playback is broken.
            // yt-dlp cannot solve YouTube's challenges without one, and the
            // failure mode without it is a confusing challenge error — so name
            // the runtime, and fail when there is none.
            let js = audio::resolver::js_runtime_args();
            match js.last() {
                Some(spec) => println!("ok      JavaScript runtime: {spec}"),
                None => failures.push(
                    "no JavaScript runtime found — install node, deno or quickjs".to_string(),
                ),
            }

            match audio::resolver::locate_ytdlp() {
                Some(path) => {
                    let version = audio::resolver::ytdlp_version(&path);
                    println!(
                        "ok      yt-dlp: {} ({})",
                        path.display(),
                        version.as_deref().unwrap_or("no version reported")
                    );
                    if version.is_none() {
                        failures.push("yt-dlp did not report a version".to_string());
                    }
                    let anonymous = config::AuthConfig {
                        mode: AuthMode::None,
                        ..Default::default()
                    };
                    match rt.block_on(audio::resolver::resolve_url(&video_id, &anonymous)) {
                        Ok(_) => println!("ok      yt-dlp resolved a stream URL"),
                        Err(e) => {
                            let message = e.to_string();
                            if message.to_lowercase().contains("bot check") {
                                println!(
                                    "note    yt-dlp was bot-checked — YouTube answering, not us"
                                );
                            } else {
                                failures
                                    .push(format!("yt-dlp could not resolve a stream: {message}"));
                            }
                        }
                    }
                }
                None => failures.push("yt-dlp not found".to_string()),
            }
        }
        None => failures.push("no video id in the search results".to_string()),
    }

    // 4. The home feed, which is a different parser path (shelves rather than
    //    a track list).
    match rt.block_on(client.browse("FEmusic_home")) {
        Ok(page) if !page.shelves.is_empty() => {
            println!("ok      home feed: {} shelves", page.shelves.len())
        }
        Ok(page) => failures.push(format!("home feed parsed {} shelves", page.shelves.len())),
        Err(e) => failures.push(format!("home feed failed: {e}")),
    }

    if failures.is_empty() {
        println!("\ncanary passed");
        return Ok(());
    }
    for failure in &failures {
        eprintln!("FAIL    {failure}");
    }
    anyhow::bail!("{} check(s) failed", failures.len())
}

fn like_status_cmd(video_id: &str) -> anyhow::Result<()> {
    use ytm::Client;

    let (lang, country) = config::Config::locale();
    let rt = tokio::runtime::Runtime::new()?;
    let cfg = config::Config::load();
    let jar = rt
        .block_on(auth::resolve(&cfg))
        .ok()
        .flatten()
        .map(|r| r.jar);
    let client = Client::new(&lang, &country, jar)?;

    let (status, allowed) = rt.block_on(client.like_status(video_id))?;
    println!("{video_id}: rating={status:?} rateable={allowed}");
    Ok(())
}

fn probe(query: &str) -> anyhow::Result<()> {
    use ytm::{Client, SearchFilter};

    let (lang, country) = config::Config::locale();
    let rt = tokio::runtime::Runtime::new()?;
    let cfg = config::Config::load();
    let jar = rt
        .block_on(auth::resolve(&cfg))
        .ok()
        .flatten()
        .map(|r| r.jar);
    let client = Client::new(&lang, &country, jar)?;

    println!("== search({query:?}) [Songs] ==");
    let songs = rt.block_on(client.search(query, SearchFilter::Songs))?.0;
    for item in songs.iter().take(5) {
        let dur = item
            .duration_secs
            .map(crate::ytm::domain::fmt_duration)
            .unwrap_or_else(|| "--:--".into());
        println!("  {:>6}  {}  |  {}", dur, item.title, item.subtitle);
    }

    println!("== search({query:?}) [Albums] ==");
    let albums = rt.block_on(client.search(query, SearchFilter::Albums))?.0;
    for item in albums.iter().take(3) {
        println!("  {}  |  {}", item.title, item.subtitle);
    }

    if let Some(album) = albums.iter().find(|i| i.browse_id.is_some()) {
        let id = album.browse_id.clone().unwrap();
        println!("== browse({id}) ==");
        let page = rt.block_on(client.browse(&id))?;
        println!(
            "  {} • {} • {} tracks",
            page.kind.label(),
            page.title,
            page.tracks.len()
        );
        for track in page.tracks.iter().take(6) {
            println!(
                "    {} — {} ({})",
                track.title,
                track.artist_line(),
                track.duration_text().unwrap_or_else(|| "--:--".into())
            );
        }
    }

    Ok(())
}
