//! Applies a `recover::Decision` to the install root. Every step is safe to repeat: a run that
//! stops part way leaves a state `recover::decide` maps to the same action, so running
//! `--recover` again resumes it. No step shows a dialog; `--recover` runs unattended.

use crate::handle::FileHandle;
use crate::recover::{self, Action, Outcome};
use crate::{process, strings, util};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::{error, fs, io};

const ATTEMPTS: u32 = 8;

/// Inspect `root`, decide, and repair when the decision says to. Returns the outcome and the
/// one-line reason that was logged.
///
/// `from_setup`: the caller is Setup itself (`--from-setup`), which holds `<mutex>setup` and
/// `<mutex>-updating` for its own run, so those two mutexes are not checked. Everything else
/// that makes a repair unsafe -- a program running from the install folder, a held file --
/// still returns `Busy`.
pub fn run(
	log: &slog::Logger,
	root: &Path,
	exe: &str,
	mutex: &str,
	from_setup: bool,
) -> (Outcome, String) {
	info!(
		log,
		"Inno Updater v{} recover: root={:?}, exe={}, mutex={}, from_setup={}",
		crate::VERSION,
		root,
		exe,
		mutex,
		from_setup
	);

	let evidence = match recover::inspect(root, exe) {
		Ok(evidence) => evidence,
		Err(err) => {
			let reason = format!("could not read the install folder: {}", err);
			error!(log, "recover: {}", reason);
			return (Outcome::Error, reason);
		}
	};
	info!(
		log,
		"recover: root_exe={}, root_marker={}, stage={:?}, new_exe={}, old_exes={:?}, overlap={}, conflicts={:?}, blocked={:?}",
		evidence.root_exe,
		evidence.root_marker,
		evidence.stage,
		evidence.new_exe,
		evidence.old_exes,
		evidence.overlap.len(),
		evidence.conflicts,
		evidence.blocked
	);

	let decision = recover::decide(&evidence);
	info!(
		log,
		"recover: decision {} ({}): {}",
		decision.outcome.label(),
		decision.outcome.code(),
		decision.reason
	);

	if decision.action == Action::None {
		return (decision.outcome, decision.reason);
	}

	if let Some(why) = busy(log, root, mutex, from_setup) {
		let reason = format!("not repairing while {}", why);
		warn!(log, "recover: {}", reason);
		return (Outcome::Busy, reason);
	}

	if let Some(why) = held(root, &decision.action) {
		let reason = format!("not repairing while {}", why);
		warn!(log, "recover: {}", reason);
		return (Outcome::Busy, reason);
	}

	match apply(log, root, exe, &decision.action) {
		Ok(()) => {
			info!(log, "recover: done, {}", decision.outcome.label());
			(decision.outcome, decision.reason)
		}
		Err(err) => {
			let reason = format!(
				"repair failed: {}; what was done is kept, run --recover again to resume",
				err
			);
			error!(log, "recover: {}", reason);
			(Outcome::Error, reason)
		}
	}
}

/// Why the install folder must not be changed right now, if it must not.
fn busy(log: &slog::Logger, root: &Path, mutex: &str, from_setup: bool) -> Option<String> {
	// Setup holds `<mutex>setup` for its whole run and `<mutex>-updating` during a background
	// update (GetSetupMutex in code.iss), in the session namespace or the global one. When
	// Setup is the caller those are its own, so they say nothing about anyone else.
	if from_setup {
		info!(
			log,
			"recover: called by Setup; its own mutexes are not checked"
		);
	} else {
		for base in [format!("{}setup", mutex), format!("{}-updating", mutex)] {
			for name in [base.clone(), format!("Global\\{}", base)] {
				if mutex_exists(&name) {
					return Some(format!("Setup is running (mutex {} exists)", name));
				}
			}
		}
	}

	match process::capture_processes_under(log, root) {
		Ok(running) if running.is_empty() => None,
		Ok(running) => Some(format!(
			"{} program(s) are running from the install folder",
			running.len()
		)),
		Err(err) => Some(format!("running programs could not be listed ({})", err)),
	}
}

/// Open every old file the repair must delete, the way the delete will, before anything is
/// changed. A file another program holds delete-pending can still look like a plain file to a
/// metadata read, and the delete then fails half way; found here it is a clean `Busy`.
fn held(root: &Path, action: &Action) -> Option<String> {
	let Action::CompleteStaged { replace } = action else {
		return None;
	};
	for rel in replace {
		let path = root.join(rel);
		match FileHandle::new(&path) {
			Ok(handle) => {
				let _ = handle.close();
			}
			Err(err) => {
				let locked = err
					.downcast_ref::<io::Error>()
					.map(crate::is_skippable_lock_error)
					.unwrap_or(false);
				if locked {
					return Some(format!("{:?} is held by another program ({})", path, err));
				}
			}
		}
	}
	None
}

fn mutex_exists(name: &str) -> bool {
	use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, GetLastError};
	use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};

	let wide = strings::to_u16s(OsStr::new(name));
	unsafe {
		let handle = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, wide.as_ptr());
		if handle != 0 {
			CloseHandle(handle);
			return true;
		}
		// Exists, but belongs to someone this process may not open: still exists.
		GetLastError() == ERROR_ACCESS_DENIED
	}
}

fn apply(
	log: &slog::Logger,
	root: &Path,
	exe: &str,
	action: &Action,
) -> Result<(), Box<dyn error::Error>> {
	match action {
		Action::None => Ok(()),
		Action::CompleteStaged { replace } => complete_staged(log, root, exe, replace),
		Action::DiscardStage => discard_stage(log, root),
		Action::PromoteNewExe => rename(log, &root.join(format!("new_{}", exe)), &root.join(exe)),
		Action::RestoreOldExe { old } => rename(log, &root.join(old), &root.join(exe)),
	}
}

fn rename(log: &slog::Logger, from: &Path, to: &Path) -> Result<(), Box<dyn error::Error>> {
	info!(log, "recover: rename {:?} -> {:?}", from, to);
	util::retry_quiet(|_| fs::rename(from, to).map_err(|e| e.into()), ATTEMPTS)
}

/// Delete an old-version file the interrupted update had not reached, the way the update
/// does: free the name by moving it aside, then mark it for deletion.
fn delete_old_file(log: &slog::Logger, path: &Path) -> Result<(), Box<dyn error::Error>> {
	info!(log, "recover: delete old {:?}", path);
	util::retry_quiet(
		|_| -> Result<(), Box<dyn error::Error>> {
			if !path.exists() {
				return Ok(());
			}
			let handle = FileHandle::new(path)?;
			if let Err(err) = handle.rename_aside() {
				warn!(log, "{}", err);
			}
			let marked = handle.mark_for_deletion();
			let closed = handle.close();
			marked?;
			closed
		},
		ATTEMPTS,
	)
}

fn complete_staged(
	log: &slog::Logger,
	root: &Path,
	exe: &str,
	replace: &[PathBuf],
) -> Result<(), Box<dyn error::Error>> {
	let stage = root.join(recover::STAGE_DIR);

	// 1. Free every path the new version needs. These are old files only: a file this update
	//    already moved is no longer in the stage, so it cannot be listed here.
	for rel in replace {
		delete_old_file(log, &root.join(rel))?;
	}

	// 2. The executable first: once it is in place, an interruption leaves "executable in the
	//    root, not in the stage, no overlap", which decides to finish the move again.
	let mut names: Vec<std::ffi::OsString> = Vec::new();
	for entry in fs::read_dir(&stage)? {
		names.push(entry?.file_name());
	}
	names.sort_by_key(|name| !name.to_string_lossy().eq_ignore_ascii_case(exe));

	for name in names {
		let src = stage.join(&name);
		let dst = root.join(&name);
		info!(log, "recover: move {:?} -> {:?}", src, dst);
		util::retry_quiet(|_| crate::move_into_place(log, &src, &dst), ATTEMPTS)?;
	}

	// 3. The stage is empty now, apart from folders a merge could not remove.
	let left = count_files(&stage)?;
	if left > 0 {
		return Err(io::Error::other(format!(
			"{} file(s) are still in {:?} after the move",
			left, stage
		))
		.into());
	}
	if let Err(err) = util::retry_quiet(
		|_| fs::remove_dir_all(&stage).map_err(|e| e.into()),
		ATTEMPTS,
	) {
		warn!(
			log,
			"recover: could not remove the empty stage {:?}: {}", stage, err
		);
	}

	// 4. As do_update does: point the uninstaller's records at the root. Rebasing skips paths
	//    that do not start with the stage, so a second run changes nothing.
	let uninstdat = root.join("unins000.dat");
	if uninstdat.is_file()
		&& let Err(err) = crate::patch_uninstdat(log, &uninstdat, &stage)
	{
		warn!(log, "recover: failed to patch {:?}: {}", uninstdat, err);
	}

	Ok(())
}

fn count_files(dir: &Path) -> io::Result<usize> {
	if !dir.exists() {
		return Ok(0);
	}
	let mut n = 0;
	for entry in fs::read_dir(dir)? {
		let entry = entry?;
		if entry.file_type()?.is_dir() {
			n += count_files(&entry.path())?;
		} else if !crate::handle::is_aside_name(&entry.file_name().to_string_lossy()) {
			n += 1;
		}
	}
	Ok(n)
}

fn discard_stage(log: &slog::Logger, root: &Path) -> Result<(), Box<dyn error::Error>> {
	let stage = root.join(recover::STAGE_DIR);

	// Move the stage out of the way first, in one rename. A deletion that stops part way
	// must not leave a smaller `_` behind: that could read as an update that was moving.
	let mut aside = root.join(format!("_.discarded-{}", std::process::id()));
	let mut n = 0;
	while aside.exists() {
		n += 1;
		aside = root.join(format!("_.discarded-{}-{}", std::process::id(), n));
	}
	rename(log, &stage, &aside)?;

	if let Err(err) = util::retry_quiet(
		|_| fs::remove_dir_all(&aside).map_err(|e| e.into()),
		ATTEMPTS,
	) {
		// Not a failure: it is no longer the stage, and the next update deletes it with the
		// rest of the old version.
		warn!(
			log,
			"recover: could not delete {:?} ({}); the next update removes it", aside, err
		);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::recover::{STAGE_DIR, Stage, decide, inspect};
	use slog::{Drain, o};
	use tempfile::tempdir;

	const EXE: &str = "Aira.exe";

	fn logger() -> slog::Logger {
		let decorator = slog_term::PlainDecorator::new(std::io::sink());
		let drain = slog_term::FullFormat::new(decorator).build().fuse();
		slog::Logger::root(std::sync::Mutex::new(drain).fuse(), o!())
	}

	// A mutex name nothing else creates, so `busy` only sees this test's processes.
	// Unique per call: tests run in parallel and some of them hold the mutex they name.
	fn mutex() -> String {
		use std::sync::atomic::{AtomicU32, Ordering};
		static NEXT: AtomicU32 = AtomicU32::new(0);
		format!(
			"inno-updater-recover-test-{}-{}",
			std::process::id(),
			NEXT.fetch_add(1, Ordering::Relaxed)
		)
	}

	fn write(path: &Path, body: &str) {
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, body).unwrap();
	}

	fn read(path: &Path) -> String {
		fs::read_to_string(path).unwrap()
	}

	// The shape of a real install: root files, a nested resources tree, and what the update
	// never touches (tools, unins*).
	fn version(base: &Path, body: &str) {
		write(&base.join(EXE), body);
		write(&base.join("icudtl.dat"), body);
		write(&base.join("locales").join("en-US.pak"), body);
		write(
			&base.join("resources").join("app").join("package.json"),
			body,
		);
		write(
			&base
				.join("resources")
				.join("app")
				.join("out")
				.join("main.js"),
			body,
		);
		write(
			&base.join("resources").join("app").join("product.json"),
			body,
		);
	}

	fn keepers(root: &Path) {
		write(&root.join("tools").join("inno_updater.exe"), "me");
		write(&root.join("unins000.exe"), "uninstaller");
	}

	fn assert_new_version(root: &Path) {
		assert_eq!(read(&root.join(EXE)), "new");
		assert_eq!(read(&root.join("icudtl.dat")), "new");
		assert_eq!(read(&root.join("locales").join("en-US.pak")), "new");
		assert_eq!(
			read(&root.join("resources").join("app").join("package.json")),
			"new"
		);
		assert_eq!(
			read(
				&root
					.join("resources")
					.join("app")
					.join("out")
					.join("main.js")
			),
			"new"
		);
		assert_eq!(
			read(&root.join("resources").join("app").join("product.json")),
			"new"
		);
		assert_eq!(read(&root.join("tools").join("inno_updater.exe")), "me");
		assert_eq!(read(&root.join("unins000.exe")), "uninstaller");
		assert!(!root.join(STAGE_DIR).exists(), "the stage is gone");
	}

	fn recover_twice(root: &Path, first: Outcome) {
		let log = logger();
		let (outcome, reason) = run(&log, root, EXE, &mutex(), false);
		assert_eq!(outcome, first, "{}", reason);
		let (again, reason) = run(&log, root, EXE, &mutex(), false);
		assert_eq!(
			again,
			Outcome::NothingToDo,
			"a second run changes nothing: {}",
			reason
		);
	}

	#[test]
	fn a_healthy_install_is_not_touched() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		version(root, "old");
		keepers(root);
		let (outcome, _) = run(&logger(), root, EXE, &mutex(), false);
		assert_eq!(outcome, Outcome::NothingToDo);
		assert_eq!(read(&root.join(EXE)), "old");
	}

	/// The updater deleted the root files (the executable first) and stopped before the
	/// subfolders: old files still sit at paths the new version uses.
	#[test]
	fn finishes_an_update_interrupted_while_deleting() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		version(root, "old");
		keepers(root);
		version(&root.join(STAGE_DIR), "new");
		fs::remove_file(root.join(EXE)).unwrap();
		fs::remove_file(root.join("icudtl.dat")).unwrap();
		write(&root.join("old-only.dll"), "old");

		recover_twice(root, Outcome::Completed);
		assert_new_version(root);
		// Not attributable to either version, so recover leaves it.
		assert!(root.join("old-only.dll").exists());
	}

	/// Deletion finished, the move had placed the executable and stopped.
	#[test]
	fn finishes_an_update_interrupted_while_moving() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		keepers(root);
		version(&root.join(STAGE_DIR), "new");
		fs::rename(root.join(STAGE_DIR).join(EXE), root.join(EXE)).unwrap();
		fs::rename(
			root.join(STAGE_DIR).join("icudtl.dat"),
			root.join("icudtl.dat"),
		)
		.unwrap();

		recover_twice(root, Outcome::Completed);
		assert_new_version(root);
	}

	/// A folder that survived deletion (a held husk inside) was being merged into.
	#[test]
	fn finishes_a_merge_into_a_surviving_folder() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		keepers(root);
		version(&root.join(STAGE_DIR), "new");
		for name in [EXE, "icudtl.dat", "locales"] {
			fs::rename(root.join(STAGE_DIR).join(name), root.join(name)).unwrap();
		}
		let app = root.join("resources").join("app");
		write(&app.join("main.js.deleting-4-0"), "husk");
		fs::rename(
			root.join(STAGE_DIR)
				.join("resources")
				.join("app")
				.join("package.json"),
			app.join("package.json"),
		)
		.unwrap();

		recover_twice(root, Outcome::Completed);
		assert_new_version(root);
	}

	/// The move had placed everything and only the empty stage was left.
	#[test]
	fn removes_an_empty_stage_left_after_the_move() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		version(root, "new");
		keepers(root);
		fs::create_dir_all(root.join(STAGE_DIR).join("resources")).unwrap();

		recover_twice(root, Outcome::Completed);
		assert_new_version(root);
	}

	/// Setup stopped while writing the stage (before the executable). The updater never ran.
	#[test]
	fn discards_a_stage_that_never_finished() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		version(root, "old");
		keepers(root);
		write(&root.join(STAGE_DIR).join("icudtl.dat"), "new");
		write(
			&root.join(STAGE_DIR).join("locales").join("en-US.pak"),
			"new",
		);

		recover_twice(root, Outcome::RolledBack);
		assert_eq!(read(&root.join(EXE)), "old");
		assert_eq!(read(&root.join("icudtl.dat")), "old");
		assert_eq!(
			read(&root.join("resources").join("app").join("product.json")),
			"old"
		);
		assert!(!root.join(STAGE_DIR).exists());
		let leftovers: Vec<_> = fs::read_dir(root)
			.unwrap()
			.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
			.filter(|n| n.starts_with("_."))
			.collect();
		assert!(
			leftovers.is_empty(),
			"the discarded stage is deleted: {:?}",
			leftovers
		);
	}

	/// A complete stage that was never applied (the session ended first) is left for Setup.
	#[test]
	fn leaves_a_complete_unapplied_stage_alone() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		version(root, "old");
		keepers(root);
		version(&root.join(STAGE_DIR), "new");

		let (outcome, _) = run(&logger(), root, EXE, &mutex(), false);
		assert_eq!(outcome, Outcome::NothingToDo);
		assert_eq!(read(&root.join(EXE)), "old");
		assert_eq!(read(&root.join(STAGE_DIR).join(EXE)), "new");
	}

	#[test]
	fn refuses_when_neither_version_is_usable() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		version(root, "old");
		keepers(root);
		write(&root.join(STAGE_DIR).join(EXE), "new");
		fs::remove_file(root.join(EXE)).unwrap();

		let (outcome, _) = run(&logger(), root, EXE, &mutex(), false);
		assert_eq!(outcome, Outcome::Refused);
		assert_eq!(
			read(&root.join(STAGE_DIR).join(EXE)),
			"new",
			"nothing is changed"
		);
		assert_eq!(read(&root.join("icudtl.dat")), "old", "nothing is changed");
	}

	#[test]
	fn versioned_swap_is_finished() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		write(&root.join("old_Aira.exe"), "old");
		write(&root.join("new_Aira.exe"), "new");
		recover_twice(root, Outcome::Completed);
		assert_eq!(read(&root.join(EXE)), "new");
		assert_eq!(
			read(&root.join("old_Aira.exe")),
			"old",
			"gc removes it, not recover"
		);
	}

	#[test]
	fn versioned_swap_is_rolled_back() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		write(&root.join("old_Aira.exe"), "old");
		recover_twice(root, Outcome::RolledBack);
		assert_eq!(read(&root.join(EXE)), "old");
	}

	/// Not a test: the process `a_running_program_blocks_the_repair` runs from the install
	/// folder. Ignored, so it only runs when asked for by name.
	#[test]
	#[ignore]
	fn busy_test_sleeper() {
		std::thread::sleep(std::time::Duration::from_secs(60));
	}

	/// A program running from the install folder (a copy of this test binary, running
	/// `busy_test_sleeper`) blocks any repair; the decision is still logged, nothing changes.
	#[test]
	fn a_running_program_blocks_the_repair() {
		// Under the project, not %TEMP%: CI's temp folder is an 8.3 short path
		// (RUNNER~1) and the process walk compares against the long image path.
		let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
		fs::create_dir_all(&base).unwrap();
		let dir = tempfile::Builder::new()
			.prefix("recover-busy")
			.tempdir_in(&base)
			.unwrap();
		let root = dir.path();
		write(&root.join("old_Aira.exe"), "old");

		let copy = root.join("bin").join("sleeper.exe");
		fs::create_dir_all(copy.parent().unwrap()).unwrap();
		fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
		let mut child = std::process::Command::new(&copy)
			.args(["repair::tests::busy_test_sleeper", "--exact", "--ignored"])
			.stdout(std::process::Stdio::null())
			.spawn()
			.unwrap();

		let (outcome, reason) = run(&logger(), root, EXE, &mutex(), false);
		let _ = child.kill();
		let _ = child.wait();
		assert_eq!(outcome, Outcome::Busy, "{}", reason);
		assert!(!root.join(EXE).exists(), "nothing is changed while busy");
	}

	#[test]
	fn a_setup_mutex_blocks_the_repair() {
		use windows_sys::Win32::Foundation::CloseHandle;
		use windows_sys::Win32::System::Threading::CreateMutexW;

		let dir = tempdir().unwrap();
		let root = dir.path();
		write(&root.join("old_Aira.exe"), "old");

		let name = mutex();
		let wide = strings::to_u16s(OsStr::new(&format!("{}-updating", name)));
		let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
		assert!(handle != 0);

		let (outcome, _) = run(&logger(), root, EXE, &name, false);
		unsafe { CloseHandle(handle) };
		assert_eq!(outcome, Outcome::Busy);
		assert!(!root.join(EXE).exists());
	}

	fn hold_setup_mutex(name: &str) -> isize {
		use windows_sys::Win32::System::Threading::CreateMutexW;
		let wide = strings::to_u16s(OsStr::new(&format!("{}-updating", name)));
		let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
		assert!(handle != 0);
		handle
	}

	fn release(handle: isize) {
		unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
	}

	/// Setup runs --recover while it holds its own mutex: the mutex alone must not stop it.
	#[test]
	fn from_setup_ignores_the_setup_mutex() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		write(&root.join("old_Aira.exe"), "old");

		let name = mutex();
		let handle = hold_setup_mutex(&name);
		let (outcome, reason) = run(&logger(), root, EXE, &name, true);
		release(handle);
		assert_eq!(outcome, Outcome::RolledBack, "{}", reason);
		assert_eq!(read(&root.join(EXE)), "old");
	}

	/// --from-setup skips only the mutex: a program running from the folder still blocks.
	#[test]
	fn from_setup_still_reports_a_running_program() {
		let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
		fs::create_dir_all(&base).unwrap();
		let dir = tempfile::Builder::new()
			.prefix("recover-busy-setup")
			.tempdir_in(&base)
			.unwrap();
		let root = dir.path();
		write(&root.join("old_Aira.exe"), "old");

		let copy = root.join("bin").join("sleeper.exe");
		fs::create_dir_all(copy.parent().unwrap()).unwrap();
		fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
		let mut child = std::process::Command::new(&copy)
			.args(["repair::tests::busy_test_sleeper", "--exact", "--ignored"])
			.stdout(std::process::Stdio::null())
			.spawn()
			.unwrap();

		let name = mutex();
		let handle = hold_setup_mutex(&name);
		let (outcome, reason) = run(&logger(), root, EXE, &name, true);
		release(handle);
		let _ = child.kill();
		let _ = child.wait();
		assert_eq!(outcome, Outcome::Busy, "{}", reason);
		assert!(!root.join(EXE).exists(), "nothing is changed while busy");
	}

	/// --from-setup skips only the mutex: a file held delete-pending at a name the new
	/// version needs still blocks.
	#[test]
	fn from_setup_still_reports_a_held_file() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		version(root, "old");
		version(&root.join(STAGE_DIR), "new");
		fs::remove_file(root.join(EXE)).unwrap();

		// Delete-pending under its own name while another handle keeps it open.
		let held = root.join("icudtl.dat");
		let holder = fs::File::open(&held).unwrap();
		let handle = FileHandle::new(&held).unwrap();
		handle.mark_for_deletion().unwrap();
		handle.close().unwrap();

		let name = mutex();
		let setup = hold_setup_mutex(&name);
		let (outcome, reason) = run(&logger(), root, EXE, &name, true);
		release(setup);
		drop(holder);
		assert_eq!(outcome, Outcome::Busy, "{}", reason);
		assert_eq!(
			read(&root.join(STAGE_DIR).join(EXE)),
			"new",
			"nothing is changed"
		);
	}

	#[test]
	fn evidence_of_a_real_interrupted_delete_matches_the_table() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		version(root, "old");
		version(&root.join(STAGE_DIR), "new");
		fs::remove_file(root.join(EXE)).unwrap();
		let e = inspect(root, EXE).unwrap();
		assert!(matches!(
			e.stage,
			Stage::Present {
				exe: true,
				marker: true,
				..
			}
		));
		assert_eq!(decide(&e).outcome, Outcome::Completed);
	}
}
