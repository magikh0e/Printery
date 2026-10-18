// Printery desktop shell
// Copyright (C) 2026 magikh0e - GPL-3.0-or-later
//
// The whole app is one HTML file that also runs in a browser. Everything the
// browser build does through the File System Access API is done here through
// these commands instead, which is why they map one-to-one onto the `FS`
// module in index.html rather than being a general-purpose filesystem API.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::ipc::Response;
use tauri::{WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
use tauri_plugin_updater::{Error as UpdaterError, UpdaterExt};
use walkdir::WalkDir;

#[derive(Serialize)]
pub struct Entry {
    path: String, // relative to the root, forward slashes, matching the browser build
    name: String,
    size: u64,
    mtime: u64, // epoch ms
}

#[derive(Serialize)]
pub struct WalkResult {
    files: Vec<Entry>,
    skipped_dirs: Vec<String>,
    truncated: bool,
}

/// Native folder picker.
///
/// Must be `async`, and must not use the plugin's `blocking_` variant. A sync
/// command runs on the main thread, and on GTK that is the same thread the
/// dialog needs in order to pump its own events, so blocking there deadlocks
/// the whole window the moment you click Add Folder. Windows and macOS survive
/// it because their native dialogs run their own message loop, which is why
/// this only ever showed up on Linux.
///
/// As an async command this runs on a worker thread, the dialog is opened
/// without blocking, and the result comes back over a channel.
#[tauri::command]
async fn pv_pick_folder(app: tauri::AppHandle) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog().file().pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    rx.recv()
        .ok()
        .flatten()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().to_string())
}

fn to_ms(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Walk a root and return every file in one call. `skip` is a list of
/// lowercase directory names to prune, matching the browser build's skip list.
/// `max` guards against a mistakenly selected drive root.
#[tauri::command]
async fn pv_walk(root: String, skip: Vec<String>, max: Option<usize>) -> Result<WalkResult, String> {
    let base = PathBuf::from(&root);
    if !base.is_dir() {
        return Err(format!("Not a folder: {}", root));
    }
    let cap = max.unwrap_or(2_000_000);
    let mut files = Vec::new();
    let mut skipped_dirs = Vec::new();
    let mut truncated = false;

    let walker = WalkDir::new(&base).follow_links(false).into_iter();
    let mut it = walker.filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy().to_lowercase();
        if name.starts_with('.') {
            return false;
        }
        if e.file_type().is_dir() && skip.contains(&name) {
            return false;
        }
        true
    });

    while let Some(next) = it.next() {
        match next {
            Ok(e) => {
                if !e.file_type().is_file() {
                    continue;
                }
                if files.len() >= cap {
                    truncated = true;
                    break;
                }
                let md = match e.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                // Join the real path components rather than replacing every
                // backslash with a slash. On Windows the separator is a
                // backslash so the two look the same, but on Linux a backslash
                // is an ordinary character in a filename, and rewriting it
                // split one file into a folder plus a file that does not
                // exist. `a\b.zip` became `a/b.zip`, which then indexed as a
                // model folder named `a` holding `b.zip`.
                let rel = match e.path().strip_prefix(&base) {
                    Ok(r) => r
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/"),
                    Err(_) => continue,
                };
                files.push(Entry {
                    name: e.file_name().to_string_lossy().to_string(),
                    path: rel,
                    size: md.len(),
                    mtime: md.modified().map(to_ms).unwrap_or(0),
                });
            }
            // An unreadable folder is reported rather than silently swallowed,
            // the same way the browser build now reports them.
            Err(err) => {
                let p = err
                    .path()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| "(unknown)".into());
                skipped_dirs.push(format!("{}: {}", p, err));
            }
        }
    }

    Ok(WalkResult {
        files,
        skipped_dirs,
        truncated,
    })
}

fn joined(root: &str, rel: &str) -> Result<PathBuf, String> {
    let mut p = PathBuf::from(root);
    // Split on '/' only, because that is what pv_walk joins with on every
    // platform, and because a backslash is a perfectly legal character in a
    // Linux filename. Splitting on it there would break real files.
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        // Every segment has to be one plain name and nothing else. That rules
        // out "..", and it also rules out the subtler case: PathBuf::push
        // throws away everything it already holds if what you hand it is
        // absolute or carries a drive prefix, so a lone segment of
        // "C:\Windows" would silently move the whole operation outside the
        // root. On Windows that parses as two components and is refused here;
        // on Linux it is just an oddly named file, which is the right answer
        // on both.
        let mut parts = Path::new(seg).components();
        match (parts.next(), parts.next()) {
            (Some(std::path::Component::Normal(_)), None) => {}
            _ => return Err("Path traversal rejected".into()),
        }
        p.push(seg);
    }
    Ok(p)
}

/// Whole file as raw bytes. Returned as an ArrayBuffer, not a JSON number array.
#[tauri::command]
async fn pv_read(root: String, rel: String) -> Result<Response, String> {
    let p = joined(&root, &rel)?;
    fs::read(&p).map(Response::new).map_err(|e| e.to_string())
}

/// A byte range, so a 200 MB archive can be indexed by reading only its tail.
#[tauri::command]
async fn pv_read_range(root: String, rel: String, start: u64, len: u64) -> Result<Response, String> {
    let p = joined(&root, &rel)?;
    let mut f = fs::File::open(&p).map_err(|e| e.to_string())?;
    let total = f.metadata().map_err(|e| e.to_string())?.len();
    if start >= total {
        return Ok(Response::new(Vec::new()));
    }
    let want = len.min(total - start) as usize;
    f.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; want];
    let mut read = 0usize;
    while read < want {
        match f.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) => return Err(e.to_string()),
        }
    }
    buf.truncate(read);
    Ok(Response::new(buf))
}

#[tauri::command]
async fn pv_stat(root: String, rel: String) -> Result<Option<Entry>, String> {
    let p = joined(&root, &rel)?;
    match fs::metadata(&p) {
        Ok(md) if md.is_file() => Ok(Some(Entry {
            name: p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            path: rel,
            size: md.len(),
            mtime: md.modified().map(to_ms).unwrap_or(0),
        })),
        _ => Ok(None),
    }
}

#[tauri::command]
async fn pv_write(root: String, rel: String, data: Vec<u8>) -> Result<(), String> {
    let p = joined(&root, &rel)?;
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&p, data).map_err(|e| e.to_string())
}

#[tauri::command]
async fn pv_remove(root: String, rel: String) -> Result<(), String> {
    let p = joined(&root, &rel)?;
    fs::remove_file(&p).map_err(|e| e.to_string())
}

/// Remove a directory, but only if it is already empty.
///
/// Deliberately remove_dir and not remove_dir_all. Deleting a model empties
/// its folder and leaving the husk behind is untidy, but a recursive delete
/// here would be a loaded gun pointed at somebody's library. Non-recursive
/// cannot take anything with it: if the folder still holds something, this
/// simply fails and the caller shrugs.
#[tauri::command]
async fn pv_remove_dir(root: String, rel: String) -> Result<(), String> {
    let p = joined(&root, &rel)?;
    if !p.is_dir() {
        return Err("Not a folder".into());
    }
    fs::remove_dir(&p).map_err(|e| e.to_string())
}

/// Move within the same root. Falls back to copy-then-delete when the source
/// and destination are on different filesystems.
#[tauri::command]
async fn pv_move(root: String, from: String, to: String) -> Result<(), String> {
    let src = joined(&root, &from)?;
    let dst = joined(&root, &to)?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if fs::rename(&src, &dst).is_ok() {
        return Ok(());
    }
    fs::copy(&src, &dst).map_err(|e| e.to_string())?;
    let a = fs::metadata(&src).map_err(|e| e.to_string())?.len();
    let b = fs::metadata(&dst).map_err(|e| e.to_string())?.len();
    if a != b {
        let _ = fs::remove_file(&dst);
        return Err("Copy verification failed, original left alone".into());
    }
    fs::remove_file(&src).map_err(|e| e.to_string())
}

#[tauri::command]
async fn pv_exists(root: String, rel: String) -> Result<bool, String> {
    Ok(joined(&root, &rel)?.exists())
}

/// Hand a file to whatever the OS uses for it, which is how "open in slicer"
/// works. The browser build cannot do this at all.
#[tauri::command]
fn pv_open(root: String, rel: String, reveal: bool) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let p = joined(&root, &rel)?;
    let app = APP.get().ok_or("App handle unavailable")?;
    if reveal {
        app.opener()
            .reveal_item_in_dir(&p)
            .map_err(|e| e.to_string())
    } else {
        app.opener()
            .open_path(p.to_string_lossy().to_string(), None::<&str>)
            .map_err(|e| e.to_string())
    }
}


/// Hand a file to a specific program, typically a slicer.
///
/// `exe` empty means "whatever the OS opens this with", which for a .3mf or
/// .stl is usually the slicer anyway. Pointing at one explicitly matters when
/// several are installed and the file association is not the one you want.
#[tauri::command]
async fn pv_open_with(root: String, rel: String, exe: String) -> Result<(), String> {
    let p = joined(&root, &rel)?;
    if !p.is_file() {
        return Err("That file is no longer there".into());
    }
    if exe.trim().is_empty() {
        return pv_open(root, rel, false);
    }
    let program = PathBuf::from(exe.trim());
    if !program.exists() {
        return Err(format!("No program at {}", program.display()));
    }
    /* On macOS an application is a bundle, which is a directory rather than
       something you can exec. Handing /Applications/BambuStudio.app straight
       to Command::new fails with EACCES, "Permission denied (os error 13)",
       which reads like a sandbox problem and is not one. Bundles have to go
       through Launch Services instead.

       The bundle check matters: the file picker puts no filter on macOS, so
       somebody can just as well point this at a plain UNIX binary, and those
       still have to be run directly. Diagnosed by Howard Aron, who hit it with
       Bambu Studio and sent the fix. */
    #[cfg(target_os = "macos")]
    let spawned = {
        let is_bundle = program.is_dir()
            || program
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("app"));
        if is_bundle {
            std::process::Command::new("/usr/bin/open")
                .arg("-a")
                .arg(&program)
                .arg(&p)
                .spawn()
        } else {
            std::process::Command::new(&program).arg(&p).spawn()
        }
    };

    // Spawned, not waited on. A slicer takes seconds to appear and the window
    // should not sit frozen until someone closes it.
    #[cfg(not(target_os = "macos"))]
    let spawned = std::process::Command::new(&program).arg(&p).spawn();

    spawned
        .map(|_| ())
        .map_err(|e| format!("Could not start {}: {}", program.display(), e))
}

/// Hand files to the system share sheet.
///
/// macOS only. Windows has an equivalent but reaching it means going through
/// IDataTransferManagerInterop, and Linux has no standard sheet at all, so the
/// button is hidden off macOS rather than pretending. The command still exists
/// on every platform so the invoke handler does not need to be conditional;
/// elsewhere it just says so.
/// One file to share. Carries its own root so a selection spanning two
/// libraries still validates each path against the folder it came from.
#[derive(serde::Deserialize)]
// Off macOS the stub ignores these, but the shape still has to deserialize so
// the frontend can call the command and get a sentence back rather than an
// argument mismatch.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct ShareItem {
    root: String,
    rel: String,
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
async fn pv_share(_items: Vec<ShareItem>) -> Result<(), String> {
    Err("The share sheet is only available on macOS".into())
}

/// The macOS one. Everything AppKit here has to happen on the main thread, so
/// the paths are resolved and checked first and only the picker itself is
/// handed over, then we wait for it to report back.
#[cfg(target_os = "macos")]
#[tauri::command]
async fn pv_share(app: tauri::AppHandle, items: Vec<ShareItem>) -> Result<(), String> {
    use tauri::Manager;

    // A whole library handed to a share sheet helps nobody and takes a while
    // to build. Well above any real selection, low enough to stay responsive.
    const MAX: usize = 60;
    if items.len() > MAX {
        return Err(format!(
            "That is {} files. Share {} or fewer at a time.",
            items.len(),
            MAX
        ));
    }

    let mut paths: Vec<String> = Vec::new();
    for it in &items {
        let p = joined(&it.root, &it.rel)?;
        // A share sheet that opens on a file that has moved is worse than an
        // error, because the failure surfaces inside Mail or Messages instead.
        if !p.is_file() {
            return Err(format!("{} is no longer there", it.rel));
        }
        paths.push(p.to_string_lossy().into_owned());
    }
    if paths.is_empty() {
        return Err("Nothing to share".into());
    }

    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "No main window".to_string())?;
    let anchor = window.clone();

    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    window
        .run_on_main_thread(move || {
            let _ = tx.send(show_share_sheet(&anchor, &paths));
        })
        .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?
}

/// Build and show the picker. Main thread only, called from pv_share.
#[cfg(target_os = "macos")]
fn show_share_sheet(window: &tauri::WebviewWindow, paths: &[String]) -> Result<(), String> {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::AnyThread; // provides alloc() on the class
    use objc2_app_kit::{NSSharingServicePicker, NSWindow};
    use objc2_foundation::{NSArray, NSPoint, NSRect, NSRectEdge, NSSize, NSString, NSURL};

    let ptr = window.ns_window().map_err(|e| e.to_string())? as *mut NSWindow;
    if ptr.is_null() {
        return Err("No native window to anchor to".into());
    }

    unsafe {
        let ns_window: &NSWindow = &*ptr;
        let view = ns_window
            .contentView()
            .ok_or_else(|| "Window has no content view".to_string())?;

        let items: Vec<Retained<AnyObject>> = paths
            .iter()
            .map(|p| {
                let url = NSURL::fileURLWithPath(&NSString::from_str(p));
                Retained::cast_unchecked::<AnyObject>(url)
            })
            .collect();
        let items = NSArray::from_retained_slice(&items);

        let picker = NSSharingServicePicker::initWithItems(NSSharingServicePicker::alloc(), &items);

        /* Anchored to the top right of the window, near where the button that
           opens it sits. AppKit's default view coordinates put the origin at
           the bottom left, so the top edge is y = height. This is the part
           most likely to want nudging once somebody sees it on a real screen. */
        let bounds = view.bounds();
        let rect = NSRect::new(
            NSPoint::new(bounds.size.width - 60.0, bounds.size.height - 4.0),
            NSSize::new(1.0, 1.0),
        );
        picker.showRelativeToRect_ofView_preferredEdge(rect, &view, NSRectEdge::MinY);
    }
    Ok(())
}

/// Pick a program, for choosing a slicer in settings.
#[tauri::command]
async fn pv_pick_exe(app: tauri::AppHandle) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut dlg = app.dialog().file();
    if cfg!(windows) {
        dlg = dlg.add_filter("Programs", &["exe"]);
    }
    dlg.pick_file(move |picked| {
        let _ = tx.send(picked);
    });
    rx.recv()
        .ok()
        .flatten()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().to_string())
}

#[derive(Serialize)]
pub struct ExtractResult {
    written: usize,
    failed: usize,
    problems: Vec<String>,
}

fn is_7z(rel: &str) -> bool {
    rel.rsplit('.').next().is_some_and(|e| e.eq_ignore_ascii_case("7z"))
}

/// Where an archive entry may be written, relative to the destination.
///
/// The zip crate has enclosed_name() for this; the 7z reader hands back raw
/// names, so this applies the same rule joined() uses on every other path in
/// this file: each segment has to be one plain name, which rules out "..",
/// absolute paths and Windows drive prefixes. A leading separator is dropped
/// rather than refused, which lands the entry inside the destination.
fn safe_entry_path(name: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for seg in name.split(['/', '\\']).filter(|s| !s.is_empty()) {
        let mut parts = Path::new(seg).components();
        match (parts.next(), parts.next()) {
            (Some(std::path::Component::Normal(_)), None) => out.push(seg),
            _ => return None,
        }
    }
    if out.as_os_str().is_empty() { None } else { Some(out) }
}

/// What is inside a 7z, without unpacking it.
///
/// Zip is listed in JS by reading the central directory off the tail of the
/// file, which needs a couple of hundred kilobytes however large the archive
/// is. 7z has no equivalent trick reachable from a browser, so listing one has
/// to happen here. That is also why the browser build cannot read them at all.
#[tauri::command]
async fn pv_7z_list(root: String, rel: String) -> Result<Vec<Entry>, String> {
    let p = joined(&root, &rel)?;
    let reader = sevenz_rust2::ArchiveReader::open(&p, sevenz_rust2::Password::empty())
        .map_err(|e| format!("Could not read {}: {}", rel, e))?;
    Ok(reader
        .archive()
        .files
        .iter()
        .filter(|e| !e.is_directory())
        .map(|e| Entry {
            path: e.name().replace('\\', "/"),
            name: e.name().rsplit(['/', '\\']).next().unwrap_or("").to_string(),
            size: e.size(),
            mtime: 0,
        })
        .collect())
}

/// Unpack a 7z. Same contract as the zip path: count what was written, keep
/// going past a bad entry, and never write outside the destination.
fn extract_7z(src: &Path, dest: &Path) -> Result<ExtractResult, String> {
    let mut reader = sevenz_rust2::ArchiveReader::open(src, sevenz_rust2::Password::empty())
        .map_err(|e| format!("Could not read the archive: {}", e))?;

    let mut written = 0usize;
    let mut failed = 0usize;
    let mut problems: Vec<String> = Vec::new();

    reader
        .for_each_entries(|entry, rd| {
            if entry.is_directory() {
                return Ok(true);
            }
            let rel = match safe_entry_path(entry.name()) {
                Some(p) => p,
                None => {
                    failed += 1;
                    problems.push(format!("{} (unsafe path, refused)", entry.name()));
                    return Ok(true);
                }
            };
            let out = dest.join(&rel);
            if let Some(parent) = out.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    failed += 1;
                    problems.push(format!("{}: {}", rel.display(), e));
                    return Ok(true);
                }
            }
            let mut w = match fs::File::create(&out) {
                Ok(w) => w,
                Err(e) => {
                    failed += 1;
                    problems.push(format!("{}: {}", rel.display(), e));
                    return Ok(true);
                }
            };
            match std::io::copy(rd, &mut w) {
                // the archive's own size is the check, same as the zip path
                Ok(n) if n == entry.size() => written += 1,
                Ok(n) => {
                    failed += 1;
                    let _ = fs::remove_file(&out);
                    problems.push(format!("{}: wrote {} of {} bytes", rel.display(), n, entry.size()));
                }
                Err(e) => {
                    failed += 1;
                    let _ = fs::remove_file(&out);
                    problems.push(format!("{}: {}", rel.display(), e));
                }
            }
            Ok(true)
        })
        .map_err(|e| format!("Could not unpack the archive: {}", e))?;

    Ok(ExtractResult { written, failed, problems })
}

/// Unpack an archive into a sibling folder. Done here rather than in JS so
/// entry bytes never cross the IPC bridge, which is what made large entries
/// fragile in the browser build. Paths from the archive are sanitised before
/// anything is written.
#[tauri::command]
async fn pv_extract(root: String, archive_rel: String, dest_rel: String) -> Result<ExtractResult, String> {
    let src = joined(&root, &archive_rel)?;
    let dest = joined(&root, &dest_rel)?;

    // 7z is a different container entirely, so it gets its own reader. The zip
    // crate cannot open one and fails with something unhelpful about a missing
    // end-of-central-directory record.
    if is_7z(&archive_rel) {
        return extract_7z(&src, &dest);
    }

    let file = fs::File::open(&src).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    let mut written = 0usize;
    let mut failed = 0usize;
    let mut problems: Vec<String> = Vec::new();

    for i in 0..zip.len() {
        let mut entry = match zip.by_index(i) {
            Ok(e) => e,
            Err(e) => {
                failed += 1;
                problems.push(format!("entry {}: {}", i, e));
                continue;
            }
        };
        if entry.is_dir() {
            continue;
        }
        // enclosed_name() rejects absolute paths and any .. traversal
        let rel = match entry.enclosed_name() {
            Some(p) => p,
            None => {
                failed += 1;
                problems.push(format!("{} (unsafe path, refused)", entry.name()));
                continue;
            }
        };
        let out = dest.join(&rel);
        if let Some(parent) = out.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                failed += 1;
                problems.push(format!("{}: {}", rel.display(), e));
                continue;
            }
        }
        let mut w = match fs::File::create(&out) {
            Ok(w) => w,
            Err(e) => {
                failed += 1;
                problems.push(format!("{}: {}", rel.display(), e));
                continue;
            }
        };
        match std::io::copy(&mut entry, &mut w) {
            Ok(n) => {
                // the zip's own size is the check; a short write is a failure
                if n == entry.size() {
                    written += 1;
                } else {
                    failed += 1;
                    let _ = fs::remove_file(&out);
                    problems.push(format!(
                        "{}: wrote {} of {} bytes",
                        rel.display(),
                        n,
                        entry.size()
                    ));
                }
            }
            Err(e) => {
                failed += 1;
                let _ = fs::remove_file(&out);
                problems.push(format!("{}: {}", rel.display(), e));
            }
        }
    }

    Ok(ExtractResult {
        written,
        failed,
        problems,
    })
}

/// Confirm a stored root still exists, so a disconnected share is reported
/// rather than looking like an empty folder.
#[tauri::command]
async fn pv_root_ok(root: String) -> bool {
    Path::new(&root).is_dir()
}


/* ---------------------------------------------------------------
   Sending a sliced file to a printer

   Moonraker (Klipper, and the Creality K2 among others) and OctoPrint both
   take a multipart upload over plain HTTP on the local network, which is why
   this lives in Rust: the webview will not post to an unencrypted box on your
   LAN, and a browser page never could.
   --------------------------------------------------------------- */

#[derive(Serialize)]
pub struct PrinterInfo {
    flavour: String,
    name: String,
    state: String,
}

fn base_url(addr: &str) -> String {
    let a = addr.trim().trim_end_matches('/');
    if a.starts_with("http://") || a.starts_with("https://") {
        a.to_string()
    } else {
        format!("http://{}", a)
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .connect_timeout(std::time::Duration::from_secs(6))
        .build()
        .map_err(|e| e.to_string())
}

/// Work out what is answering at that address, so a typo says so plainly
/// rather than failing later halfway through an upload.
#[tauri::command]
async fn pv_printer_probe(addr: String, api_key: Option<String>) -> Result<PrinterInfo, String> {
    let base = base_url(&addr);
    let c = client()?;

    if let Ok(r) = c.get(format!("{}/printer/info", base)).send().await {
        if r.status().is_success() {
            let v: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
            let res = &v["result"];
            return Ok(PrinterInfo {
                flavour: "moonraker".into(),
                name: res["hostname"].as_str().unwrap_or("Klipper").to_string(),
                state: res["state"].as_str().unwrap_or("unknown").to_string(),
            });
        }
    }

    let mut req = c.get(format!("{}/api/version", base));
    if let Some(k) = api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.header("X-Api-Key", k);
    }
    if let Ok(r) = req.send().await {
        if r.status().is_success() {
            let v: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
            return Ok(PrinterInfo {
                flavour: "octoprint".into(),
                name: format!("OctoPrint {}", v["server"].as_str().unwrap_or("")),
                state: "ready".into(),
            });
        }
        return Err("Reached it, but it did not answer as Moonraker or OctoPrint. Wrong port, or an API key is needed".into());
    }
    Err("Nothing answered at that address".into())
}

/// Upload a sliced file, optionally starting it.
#[tauri::command]
async fn pv_printer_send(
    root: String,
    rel: String,
    addr: String,
    flavour: String,
    api_key: Option<String>,
    start: bool,
) -> Result<String, String> {
    let p = joined(&root, &rel)?;
    let name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or("Bad file name")?;
    let bytes = tokio::fs::read(&p).await.map_err(|e| e.to_string())?;
    let base = base_url(&addr);
    let c = client()?;

    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(name.clone())
        .mime_str("application/octet-stream")
        .map_err(|e| e.to_string())?;

    let (url, form) = if flavour == "octoprint" {
        let mut f = reqwest::multipart::Form::new().part("file", part);
        if start {
            f = f.text("print", "true");
        }
        (format!("{}/api/files/local", base), f)
    } else {
        let mut f = reqwest::multipart::Form::new().part("file", part);
        f = f.text("root", "gcodes");
        if start {
            f = f.text("print", "true");
        }
        (format!("{}/server/files/upload", base), f)
    };

    let mut req = c.post(url).multipart(form);
    if let Some(k) = api_key.as_deref().filter(|k| !k.is_empty()) {
        req = req.header("X-Api-Key", k);
    }
    let r = req.send().await.map_err(|e| e.to_string())?;
    let code = r.status();
    let body = r.text().await.unwrap_or_default();
    if !code.is_success() {
        return Err(format!("Printer refused it ({}): {}", code.as_u16(), body.chars().take(200).collect::<String>()));
    }
    Ok(name)
}


#[derive(Serialize)]
pub struct JobStatus {
    state: String,      // printing | paused | complete | cancelled | standby | error | offline
    filename: String,
    progress: f64,      // 0..1
    elapsed_s: f64,
    remaining_s: f64,
    filament_mm: f64,
    message: String,
}

/// What the printer is doing right now.
///
/// Deliberately forgiving: a printer that is off, asleep or mid-reboot is a
/// normal state to be in, not an error worth shouting about, so anything
/// unreachable comes back as "offline".
#[tauri::command]
async fn pv_printer_status(addr: String, flavour: String, api_key: Option<String>) -> JobStatus {
    let offline = |msg: &str| JobStatus {
        state: "offline".into(), filename: String::new(), progress: 0.0,
        elapsed_s: 0.0, remaining_s: 0.0, filament_mm: 0.0, message: msg.into(),
    };
    let base = base_url(&addr);
    let c = match client() { Ok(c) => c, Err(e) => return offline(&e) };

    if flavour == "octoprint" {
        let mut req = c.get(format!("{}/api/job", base));
        if let Some(k) = api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.header("X-Api-Key", k);
        }
        let r = match req.send().await { Ok(r) => r, Err(e) => return offline(&e.to_string()) };
        let v: serde_json::Value = match r.json().await { Ok(v) => v, Err(e) => return offline(&e.to_string()) };
        let st = v["state"].as_str().unwrap_or("").to_lowercase();
        return JobStatus {
            state: if st.contains("printing") { "printing".into() }
                   else if st.contains("paused") { "paused".into() }
                   else if st.contains("operational") { "standby".into() }
                   else if st.contains("error") { "error".into() }
                   else { st },
            filename: v["job"]["file"]["name"].as_str().unwrap_or("").to_string(),
            progress: v["progress"]["completion"].as_f64().unwrap_or(0.0) / 100.0,
            elapsed_s: v["progress"]["printTime"].as_f64().unwrap_or(0.0),
            remaining_s: v["progress"]["printTimeLeft"].as_f64().unwrap_or(0.0),
            filament_mm: v["job"]["filament"]["tool0"]["length"].as_f64().unwrap_or(0.0),
            message: String::new(),
        };
    }

    let url = format!(
        "{}/printer/objects/query?print_stats&display_status&virtual_sdcard",
        base
    );
    let r = match c.get(url).send().await { Ok(r) => r, Err(e) => return offline(&e.to_string()) };
    let v: serde_json::Value = match r.json().await { Ok(v) => v, Err(e) => return offline(&e.to_string()) };
    let st = &v["result"]["status"];
    let ps = &st["print_stats"];
    let raw = ps["state"].as_str().unwrap_or("").to_lowercase();
    let progress = st["display_status"]["progress"]
        .as_f64()
        .or_else(|| st["virtual_sdcard"]["progress"].as_f64())
        .unwrap_or(0.0);
    let elapsed = ps["print_duration"].as_f64().unwrap_or(0.0);
    // Klipper reports elapsed but not remaining, so it is inferred from progress
    let remaining = if progress > 0.02 { (elapsed / progress) - elapsed } else { 0.0 };
    JobStatus {
        state: match raw.as_str() {
            "printing" => "printing".into(),
            "paused" => "paused".into(),
            "complete" => "complete".into(),
            "cancelled" => "cancelled".into(),
            "standby" => "standby".into(),
            "error" => "error".into(),
            other => other.to_string(),
        },
        filename: ps["filename"].as_str().unwrap_or("").to_string(),
        progress,
        elapsed_s: elapsed,
        remaining_s: remaining.max(0.0),
        filament_mm: ps["filament_used"].as_f64().unwrap_or(0.0),
        message: ps["message"].as_str().unwrap_or("").to_string(),
    }
}

static APP: std::sync::OnceLock<tauri::AppHandle> = std::sync::OnceLock::new();

/// Ask the release manifest whether a newer signed build exists, and offer it.
///
/// Every failure is deliberately silent: offline, no release yet, a declined
/// prompt. Someone organising print files should never get an error dialog
/// because their network was flaky at launch.
///
/// Runs on a worker thread, so `blocking_show` is safe here. On the main
/// thread it would deadlock GTK, the same way the folder picker did.
async fn check_for_update(app: tauri::AppHandle) {
    // Startup check. Nothing to say when there is no update, but a failure is
    // worth recording: see pv_check_update for why silence is the problem.
    match run_update_check(&app).await {
        Ok(Some(_)) | Ok(None) => {}
        Err(e) => eprintln!("[Printery] update check failed: {}", e),
    }
}

/// Ask the endpoint what it has, and offer it if it is newer.
///
/// Returns the version offered, or None when already current. Every failure
/// comes back as an Err with a reason rather than being swallowed, because a
/// silent updater is indistinguishable from a working one that has nothing to
/// do, and this is the mechanism every future fix arrives through.
async fn run_update_check(app: &tauri::AppHandle) -> Result<Option<String>, String> {
    let updater = app
        .updater()
        .map_err(|e| format!("updater unavailable: {}", e))?;

    let found = updater.check().await.map_err(|e| match e {
        // The endpoint answered and the manifest parsed. This release simply
        // carries no build for the platform doing the asking, which is what
        // happens whenever a release goes out without one of the three. Worth
        // naming precisely, because the generic wording sends someone off
        // checking their network for a problem that is not there, and the
        // honest answer is that there is nothing for them to install.
        UpdaterError::TargetsNotFound(_) | UpdaterError::TargetNotFound(_) => {
            "this release has no build for your platform yet".to_string()
        }
        other => format!("could not reach the update endpoint: {}", other),
    })?;

    let update = match found {
        Some(u) => u,
        None => return Ok(None),
    };
    let offered = update.version.clone();

    let msg = format!(
        "Printery {} is available (you have {}). Install it and restart now?

Your library and index are untouched.",
        update.version, update.current_version
    );
    let accepted = app
        .dialog()
        .message(msg)
        .title("Update available")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Install".into(),
            "Later".into(),
        ))
        .blocking_show();

    if accepted {
        update
            .download_and_install(|_, _| {}, || {})
            .await
            .map_err(|e| format!("download or install failed: {}", e))?;
        app.restart();
    }
    Ok(Some(offered))
}

/// The same check, asked for on purpose from Settings.
///
/// The startup check runs once and says nothing when it fails, so there was no
/// way to tell a broken updater from an up to date one. This one always
/// answers: the version it offered, that you are current, or why it could not
/// find out.
#[tauri::command]
async fn pv_check_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    run_update_check(&app).await
}

pub fn run() {
    tauri::Builder::default()
        // Must be registered first. A second launch hands its arguments to the
        // running instance and exits, rather than opening a second window onto
        // the same database.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            pv_pick_folder,
            pv_walk,
            pv_read,
            pv_read_range,
            pv_stat,
            pv_write,
            pv_remove,
            pv_move,
            pv_exists,
            pv_open,
            pv_open_with,
            pv_pick_exe,
            pv_printer_probe,
            pv_printer_send,
            pv_printer_status,
            pv_extract,
            pv_share,
            pv_remove_dir,
            pv_check_update,
            pv_7z_list,
            pv_root_ok
        ])
        .setup(|app| {
            let _ = APP.set(app.handle().clone());

            // Tells the page it is running in the desktop shell, before any app
            // JS runs, so FS can pick its implementation at load time. The OS
            // goes with it because some things exist on one platform only, and
            // the version string alone cannot tell a Mac from a PC.
            let init = format!(
                "window.__PV_DESKTOP__ = {}; window.__PV_OS__ = {};",
                serde_json::to_string(env!("CARGO_PKG_VERSION")).unwrap(),
                serde_json::to_string(std::env::consts::OS).unwrap()
            );

            WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
                .title(concat!("Printery ", env!("CARGO_PKG_VERSION")))
                .inner_size(1280.0, 860.0)
                .min_inner_size(720.0, 520.0)
                .initialization_script(init.as_str())
                .build()?;

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move { check_for_update(handle).await });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Printery desktop app");
}
