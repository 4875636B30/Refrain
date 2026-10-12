//! Refrain — a fast, low-memory desktop client for YouTube Music.

mod app;
mod audio;
mod auth;
mod config;
mod media;
mod thumbs;
mod ui;
mod ytm;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();

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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([820.0, 520.0])
            .with_title("Refrain"),
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

    let items = rt.block_on(client.search(query, SearchFilter::All))?;
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
    match thumbs::decode(&bytes) {
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
    let songs = rt.block_on(client.search(query, SearchFilter::Songs))?;
    for item in songs.iter().take(5) {
        let dur = item
            .duration_secs
            .map(crate::ytm::domain::fmt_duration)
            .unwrap_or_else(|| "--:--".into());
        println!("  {:>6}  {}  |  {}", dur, item.title, item.subtitle);
    }

    println!("== search({query:?}) [Albums] ==");
    let albums = rt.block_on(client.search(query, SearchFilter::Albums))?;
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
