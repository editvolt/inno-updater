//! `--recover <root>`: read the install root for a background update that stopped half way,
//! and decide -- from what is on disk only -- whether it can be finished, undone, or must be
//! left alone. This module only READS; `repair` applies the decision.
//!
//! How the swap it recovers from works (the traditional, non-versioned layout):
//!
//! 1. Inno Setup stages the whole new version into `<root>\_`, in `[Files]` order: every
//!    file of the build except the executable, then `<exe>`, the visual elements manifest,
//!    `bin\*`, and LAST `resources\app\product.json`. The old version is untouched.
//! 2. Only after staging has finished does Setup run this helper, which DELETES the old
//!    version (every root entry except `_`, `tools`, `unins*`, `appx` and `bootstrap`; no
//!    backup is kept), then moves each entry of `_` into the root, removes `_`, and patches
//!    `unins000.dat`.
//!
//! The versioned layout instead stages `new_<exe>` next to `<exe>` and swaps the two with
//! `<exe> -> old_<exe>`, `new_<exe> -> <exe>`.
//!
//! What this means for recovery: in the traditional layout the old version is never moved
//! aside, so once deletion has started the only way back to a working install is forward.
//! "Rolling back" there can only mean discarding a staged copy that never finished, which is
//! only provable while the old version is still in place. The decision table is in
//! `decide`; the README has it as a table.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The folder Setup stages a traditional background update into (`GetDestDir` in code.iss).
pub const STAGE_DIR: &str = "_";

/// The last file Setup writes into the stage. Present means staging finished.
pub const STAGE_COMPLETE_MARKER: [&str; 3] = ["resources", "app", "product.json"];

/// What `--recover` reports, as the process exit code.
///
/// None of these is 1, the code every other mode exits with on failure, so a caller can tell
/// a recover result from a helper too old to know `--recover` (that one prints "Bad usage"
/// and exits 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
	/// The install is usable and nothing was changed.
	NothingToDo = 0,
	/// A half-finished update was finished.
	Completed = 10,
	/// A half-finished update was undone; the previous version is in place.
	RolledBack = 11,
	/// The on-disk state does not prove any repair safe; nothing was changed.
	Refused = 20,
	/// Setup, the updater or the application is still running, or a file is held; nothing
	/// was changed. Try again later.
	Busy = 21,
	/// A repair was attempted and failed part way, or the arguments were wrong. Every step
	/// is idempotent, so running `--recover` again resumes it.
	Error = 30,
}

impl Outcome {
	pub fn code(self) -> i32 {
		self as i32
	}

	pub fn label(self) -> &'static str {
		match self {
			Outcome::NothingToDo => "nothing-to-do",
			Outcome::Completed => "completed",
			Outcome::RolledBack => "rolled-back",
			Outcome::Refused => "refused",
			Outcome::Busy => "busy",
			Outcome::Error => "error",
		}
	}
}

/// The capability lines `--capabilities` prints, one per line.
pub const CAPABILITIES: &[&str] = &["recover"];

/// Root entries the update never deletes. Kept in step with `delete_existing_version`.
pub fn is_preserved_root_name(name: &str) -> bool {
	name == "tools" || name.starts_with("unins") || name == "appx" || name == "bootstrap"
}

/// A name the versioned swap gives the previous executable: `old_<exe>` or `old_<n>_<exe>`
/// (`find_available_old_path`).
pub fn is_old_exe_name(name: &str, exe: &str) -> bool {
	if name.len() < 4 || !name.is_char_boundary(4) || !name[..4].eq_ignore_ascii_case("old_") {
		return false;
	}
	let rest = &name[4..];
	if eq_name(rest, exe) {
		return true;
	}
	match rest.split_once('_') {
		Some((n, tail)) => {
			!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && eq_name(tail, exe)
		}
		None => false,
	}
}

// Windows file names compare without regard to ASCII case.
fn eq_name(a: &str, b: &str) -> bool {
	a.eq_ignore_ascii_case(b)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
	Absent,
	/// `_` exists but is not a directory.
	NotADirectory,
	Present {
		/// `_\<exe>` is a file.
		exe: bool,
		/// `_\resources\app\product.json` is a file.
		marker: bool,
		/// Top-level names in `_` the update would never move (tools, unins*, ...).
		preserved: Vec<String>,
	},
}

/// Everything the decision depends on, read from disk by `inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
	/// `<root>\<exe>` is a file.
	pub root_exe: bool,
	/// `<root>\resources\app\product.json` is a file.
	pub root_marker: bool,
	pub stage: Stage,
	/// Relative paths that are a regular file both in `_` and in the root. The update moves
	/// a file OUT of `_` when it moves it into place, so a path in both was not put in the
	/// root by this update: it is an old-version file the deletion had not reached.
	pub overlap: Vec<PathBuf>,
	/// Relative paths that are a file on one side and a directory (or a link) on the other.
	pub conflicts: Vec<PathBuf>,
	/// Relative paths in the root that exist but cannot be read: a file another program
	/// holds in a delete-pending state. Nothing can be moved onto such a name yet.
	pub blocked: Vec<PathBuf>,
	/// `<root>\new_<exe>` is a file (versioned layout).
	pub new_exe: bool,
	/// `old_<exe>` / `old_<n>_<exe>` files in the root (versioned layout), sorted.
	pub old_exes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
	/// Change nothing.
	None,
	/// Traditional layout: delete `replace` (old files at paths the new version uses), move
	/// every entry of `_` into the root (the executable first), remove `_`, patch
	/// `unins000.dat`.
	CompleteStaged { replace: Vec<PathBuf> },
	/// Traditional layout: the stage never finished and the old version is intact; move `_`
	/// aside and delete it so a later update does not inherit its files.
	DiscardStage,
	/// Versioned layout: `new_<exe>` -> `<exe>`.
	PromoteNewExe,
	/// Versioned layout: `<old>` -> `<exe>`.
	RestoreOldExe { old: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
	pub outcome: Outcome,
	pub action: Action,
	pub reason: String,
}

fn decision(outcome: Outcome, action: Action, reason: &str) -> Decision {
	Decision {
		outcome,
		action,
		reason: reason.to_string(),
	}
}

fn refuse(reason: &str) -> Decision {
	decision(Outcome::Refused, Action::None, reason)
}

/// The decision table. Pure: every input is in `Evidence`.
pub fn decide(e: &Evidence) -> Decision {
	let (stage_exe, stage_marker, preserved) = match &e.stage {
		Stage::NotADirectory => {
			return refuse("the stage folder `_` exists but is not a directory");
		}
		Stage::Absent => return decide_without_stage(e),
		Stage::Present {
			exe,
			marker,
			preserved,
		} => (*exe, *marker, preserved),
	};

	if e.new_exe {
		return refuse(
			"both a staged folder `_` and a staged new_<exe> exist; the two update layouts never mix",
		);
	}
	if !preserved.is_empty() {
		return refuse(&format!(
			"the stage folder holds entries the update never moves ({}); it was not written by Setup",
			preserved.join(", ")
		));
	}

	let complete = |reason: &str| -> Decision {
		if !e.conflicts.is_empty() {
			return refuse(&format!(
				"cannot finish the update: {} path(s) are a file on one side and a folder on the other, first {:?}",
				e.conflicts.len(),
				e.conflicts[0]
			));
		}
		if !e.blocked.is_empty() {
			return decision(
				Outcome::Busy,
				Action::None,
				&format!(
					"cannot finish the update yet: {} file(s) in the install folder are held by another program, first {:?}",
					e.blocked.len(),
					e.blocked[0]
				),
			);
		}
		decision(
			Outcome::Completed,
			Action::CompleteStaged {
				replace: e.overlap.clone(),
			},
			reason,
		)
	};

	match (e.root_exe, stage_exe) {
		// The installed executable is still there and the staged one has not moved. The
		// updater deletes root files before any subfolder file (breadth first), the
		// executable among the first, and moves nothing until deletion has finished: the old
		// version is in place.
		(true, true) => {
			if !e.root_marker {
				refuse(
					"the installed executable is present but resources\\app\\product.json is not, while an update is staged; not a state an interrupted update leaves",
				)
			} else if stage_marker {
				decision(
					Outcome::NothingToDo,
					Action::None,
					"a complete update is staged but was never applied; the installed version is intact and is left as it is",
				)
			} else {
				decision(
					Outcome::RolledBack,
					Action::DiscardStage,
					"staging stopped before it finished (no resources\\app\\product.json in `_`) and the updater never ran; the installed version is intact, the partial stage is discarded",
				)
			}
		}
		// The old executable is gone and the new one is still staged: deletion started, the
		// move had not reached the executable. There is no backup to go back to.
		(false, true) => {
			if stage_marker {
				complete(
					"the update was interrupted while replacing the old version; the staged version is complete, so the update is finished",
				)
			} else {
				// The update moves the stage in directory order, the executable first for
				// any product whose name sorts before its folders (and `repair` always moves
				// it first), so an incomplete stage that still holds the executable is not
				// a move that stopped: it is an unfinished stage beside a missing exe.
				refuse(
					"the installed executable is gone and the staged version is incomplete; neither version can be restored, reinstall",
				)
			}
		}
		// The executable is in the root and no longer in the stage.
		(true, false) => {
			if e.overlap.is_empty() {
				if e.root_marker || stage_marker {
					complete(
						"the update was interrupted while moving the new version into place; the move is finished",
					)
				} else {
					refuse(
						"the executable has been moved but resources\\app\\product.json is in neither the install folder nor the stage",
					)
				}
			} else if stage_marker {
				refuse(
					"old files are still in place but the staged copy has no executable although staging finished; not a state an interrupted update leaves",
				)
			} else if !e.root_marker {
				refuse(
					"staging is incomplete and the installed version is missing resources\\app\\product.json",
				)
			} else {
				decision(
					Outcome::RolledBack,
					Action::DiscardStage,
					"staging stopped before it reached the executable and the updater never ran; the installed version is intact, the partial stage is discarded",
				)
			}
		}
		(false, false) => refuse("no executable in the install folder or in the stage; reinstall"),
	}
}

fn decide_without_stage(e: &Evidence) -> Decision {
	match (e.root_exe, e.new_exe) {
		(true, false) => decision(
			Outcome::NothingToDo,
			Action::None,
			"the install is healthy; no update is in progress",
		),
		(true, true) => decision(
			Outcome::NothingToDo,
			Action::None,
			"a new_<exe> is staged but was never swapped in; the installed version is intact and is left as it is",
		),
		(false, true) => {
			if e.blocked.is_empty() {
				decision(
					Outcome::Completed,
					Action::PromoteNewExe,
					"the executable swap stopped after moving the old executable aside; the staged new executable is moved into place",
				)
			} else {
				decision(
					Outcome::Busy,
					Action::None,
					"the executable name is held by another program",
				)
			}
		}
		(false, false) => match e.old_exes.len() {
			0 => refuse("no executable in the install folder and nothing staged; reinstall"),
			1 if e.blocked.is_empty() => decision(
				Outcome::RolledBack,
				Action::RestoreOldExe {
					old: e.old_exes[0].clone(),
				},
				"the executable swap moved the old executable aside and the new one is missing; the old executable is moved back",
			),
			1 => decision(
				Outcome::Busy,
				Action::None,
				"the executable name is held by another program",
			),
			_ => refuse(&format!(
				"no executable and {} previous executables ({}); cannot tell which one is current",
				e.old_exes.len(),
				e.old_exes.join(", ")
			)),
		},
	}
}

enum Probe {
	Missing,
	File,
	Dir,
	/// A symbolic link or junction; never followed.
	Link,
	/// Exists but cannot be read (delete pending, access denied).
	Unreadable,
}

fn probe(path: &Path) -> Probe {
	match fs::symlink_metadata(path) {
		Ok(md) if md.file_type().is_symlink() => Probe::Link,
		Ok(md) if md.is_dir() => Probe::Dir,
		Ok(md) if md.is_file() => Probe::File,
		Ok(_) => Probe::Link,
		Err(err) if err.kind() == io::ErrorKind::NotFound => Probe::Missing,
		Err(_) => Probe::Unreadable,
	}
}

fn is_file(path: &Path) -> bool {
	matches!(probe(path), Probe::File)
}

fn marker_under(base: &Path) -> PathBuf {
	let mut p = base.to_path_buf();
	for part in STAGE_COMPLETE_MARKER {
		p.push(part);
	}
	p
}

/// Read the evidence for `root`, whose executable is called `exe`. Reads only.
pub fn inspect(root: &Path, exe: &str) -> io::Result<Evidence> {
	if !root.is_dir() {
		return Err(io::Error::new(
			io::ErrorKind::NotFound,
			format!("install root is not a directory: {:?}", root),
		));
	}

	let stage_path = root.join(STAGE_DIR);
	let new_exe_name = format!("new_{}", exe);

	let mut old_exes = Vec::new();
	for entry in fs::read_dir(root)? {
		let entry = entry?;
		let name = entry.file_name().to_string_lossy().into_owned();
		if is_old_exe_name(&name, exe) && entry.file_type()?.is_file() {
			old_exes.push(name);
		}
	}
	old_exes.sort();

	let mut evidence = Evidence {
		root_exe: is_file(&root.join(exe)),
		root_marker: is_file(&marker_under(root)),
		stage: Stage::Absent,
		overlap: Vec::new(),
		conflicts: Vec::new(),
		blocked: Vec::new(),
		new_exe: is_file(&root.join(&new_exe_name)),
		old_exes,
	};

	match probe(&stage_path) {
		Probe::Missing => {
			if matches!(probe(&root.join(exe)), Probe::Unreadable) {
				evidence.blocked.push(PathBuf::from(exe));
			}
		}
		Probe::Dir => {
			let mut preserved = Vec::new();
			for entry in fs::read_dir(&stage_path)? {
				let name = entry?.file_name().to_string_lossy().into_owned();
				if is_preserved_root_name(&name) {
					preserved.push(name);
				}
			}
			preserved.sort();
			evidence.stage = Stage::Present {
				exe: is_file(&stage_path.join(exe)),
				marker: is_file(&marker_under(&stage_path)),
				preserved,
			};
			compare(&stage_path, root, Path::new(""), &mut evidence)?;
		}
		_ => evidence.stage = Stage::NotADirectory,
	}

	Ok(evidence)
}

// Walk `stage\rel` and compare every entry with `root\rel`.
fn compare(stage: &Path, root: &Path, rel: &Path, e: &mut Evidence) -> io::Result<()> {
	for entry in fs::read_dir(stage.join(rel))? {
		let entry = entry?;
		let name = entry.file_name();
		if crate::handle::is_aside_name(&name.to_string_lossy()) {
			continue;
		}
		let rel_child = rel.join(&name);
		let staged = entry.file_type()?;
		let installed = probe(&root.join(&rel_child));

		if staged.is_symlink() {
			e.conflicts.push(rel_child);
		} else if staged.is_dir() {
			match installed {
				Probe::Dir => compare(stage, root, &rel_child, e)?,
				Probe::Missing => {}
				Probe::Unreadable => e.blocked.push(rel_child),
				Probe::File | Probe::Link => e.conflicts.push(rel_child),
			}
		} else {
			match installed {
				Probe::File => e.overlap.push(rel_child),
				Probe::Missing => {}
				Probe::Unreadable => e.blocked.push(rel_child),
				Probe::Dir | Probe::Link => e.conflicts.push(rel_child),
			}
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;
	use tempfile::tempdir;

	const EXE: &str = "Aira.exe";

	fn ev() -> Evidence {
		Evidence {
			root_exe: true,
			root_marker: true,
			stage: Stage::Absent,
			overlap: vec![],
			conflicts: vec![],
			blocked: vec![],
			new_exe: false,
			old_exes: vec![],
		}
	}

	fn staged(exe: bool, marker: bool) -> Stage {
		Stage::Present {
			exe,
			marker,
			preserved: vec![],
		}
	}

	fn outcome(e: &Evidence) -> Outcome {
		decide(e).outcome
	}

	// ---- decision table ----

	#[test]
	fn healthy_install_is_left_alone() {
		let d = decide(&ev());
		assert_eq!(d.outcome, Outcome::NothingToDo);
		assert_eq!(d.action, Action::None);
	}

	#[test]
	fn healthy_install_ignores_old_exe_leftovers() {
		let e = Evidence {
			old_exes: vec!["old_Aira.exe".into()],
			..ev()
		};
		assert_eq!(decide(&e).action, Action::None);
	}

	#[test]
	fn no_exe_and_nothing_staged_is_refused() {
		let e = Evidence {
			root_exe: false,
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn complete_stage_never_applied_is_left_alone() {
		let e = Evidence {
			stage: staged(true, true),
			overlap: vec![PathBuf::from(EXE)],
			..ev()
		};
		let d = decide(&e);
		assert_eq!(d.outcome, Outcome::NothingToDo);
		assert_eq!(d.action, Action::None);
	}

	#[test]
	fn stage_stopped_after_the_exe_is_discarded() {
		let e = Evidence {
			stage: staged(true, false),
			overlap: vec![PathBuf::from(EXE)],
			..ev()
		};
		let d = decide(&e);
		assert_eq!(d.outcome, Outcome::RolledBack);
		assert_eq!(d.action, Action::DiscardStage);
	}

	#[test]
	fn stage_stopped_before_the_exe_is_discarded() {
		let e = Evidence {
			stage: staged(false, false),
			overlap: vec![PathBuf::from("icudtl.dat")],
			..ev()
		};
		assert_eq!(decide(&e).action, Action::DiscardStage);
	}

	#[test]
	fn interrupted_deletion_with_a_complete_stage_is_completed() {
		let e = Evidence {
			root_exe: false,
			stage: staged(true, true),
			overlap: vec![PathBuf::from("resources/app/package.json")],
			..ev()
		};
		let d = decide(&e);
		assert_eq!(d.outcome, Outcome::Completed);
		assert_eq!(
			d.action,
			Action::CompleteStaged {
				replace: vec![PathBuf::from("resources/app/package.json")]
			}
		);
	}

	#[test]
	fn interrupted_deletion_with_an_incomplete_stage_is_refused() {
		let e = Evidence {
			root_exe: false,
			stage: staged(true, false),
			overlap: vec![PathBuf::from("icudtl.dat")],
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn interrupted_move_after_the_exe_is_completed() {
		let e = Evidence {
			stage: staged(false, true),
			root_marker: false,
			..ev()
		};
		assert_eq!(
			decide(&e).action,
			Action::CompleteStaged { replace: vec![] }
		);
	}

	#[test]
	fn incomplete_stage_beside_a_missing_exe_is_refused_even_without_overlap() {
		let e = Evidence {
			root_exe: false,
			stage: staged(true, false),
			root_marker: true,
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn empty_stage_after_the_move_is_cleaned_up() {
		let e = Evidence {
			stage: staged(false, false),
			..ev()
		};
		assert_eq!(
			decide(&e).action,
			Action::CompleteStaged { replace: vec![] }
		);
	}

	#[test]
	fn exe_in_neither_place_is_refused() {
		let e = Evidence {
			root_exe: false,
			stage: staged(false, true),
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn old_exe_without_old_resources_is_refused() {
		let e = Evidence {
			stage: staged(true, true),
			root_marker: false,
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn finished_stage_without_its_exe_beside_old_files_is_refused() {
		let e = Evidence {
			stage: staged(false, true),
			overlap: vec![PathBuf::from("icudtl.dat")],
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn a_type_conflict_refuses_completion() {
		let e = Evidence {
			root_exe: false,
			stage: staged(true, true),
			conflicts: vec![PathBuf::from("locales")],
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn a_held_file_makes_completion_wait() {
		let e = Evidence {
			root_exe: false,
			stage: staged(true, true),
			blocked: vec![PathBuf::from("resources/app/x.node")],
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Busy);
	}

	#[test]
	fn a_stage_that_is_a_file_is_refused() {
		let e = Evidence {
			stage: Stage::NotADirectory,
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn a_stage_holding_preserved_names_is_refused() {
		let e = Evidence {
			root_exe: false,
			stage: Stage::Present {
				exe: true,
				marker: true,
				preserved: vec!["tools".into()],
			},
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn mixed_layouts_are_refused() {
		let e = Evidence {
			root_exe: false,
			stage: staged(true, true),
			new_exe: true,
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn versioned_swap_stopped_mid_way_is_completed() {
		let e = Evidence {
			root_exe: false,
			new_exe: true,
			old_exes: vec!["old_Aira.exe".into()],
			..ev()
		};
		let d = decide(&e);
		assert_eq!(d.outcome, Outcome::Completed);
		assert_eq!(d.action, Action::PromoteNewExe);
	}

	#[test]
	fn versioned_staged_exe_never_swapped_is_left_alone() {
		let e = Evidence {
			new_exe: true,
			..ev()
		};
		assert_eq!(decide(&e).action, Action::None);
	}

	#[test]
	fn versioned_swap_that_lost_the_new_exe_is_rolled_back() {
		let e = Evidence {
			root_exe: false,
			old_exes: vec!["old_1_Aira.exe".into()],
			..ev()
		};
		let d = decide(&e);
		assert_eq!(d.outcome, Outcome::RolledBack);
		assert_eq!(
			d.action,
			Action::RestoreOldExe {
				old: "old_1_Aira.exe".into()
			}
		);
	}

	#[test]
	fn two_old_exes_and_no_exe_is_refused() {
		let e = Evidence {
			root_exe: false,
			old_exes: vec!["old_1_Aira.exe".into(), "old_Aira.exe".into()],
			..ev()
		};
		assert_eq!(outcome(&e), Outcome::Refused);
	}

	#[test]
	fn exit_codes_are_distinct_and_never_one() {
		let all = [
			Outcome::NothingToDo,
			Outcome::Completed,
			Outcome::RolledBack,
			Outcome::Refused,
			Outcome::Busy,
			Outcome::Error,
		];
		let codes: Vec<i32> = all.iter().map(|o| o.code()).collect();
		assert_eq!(codes, vec![0, 10, 11, 20, 21, 30]);
		assert!(!codes.contains(&1));
	}

	#[test]
	fn old_exe_names() {
		assert!(is_old_exe_name("old_Aira.exe", EXE));
		assert!(is_old_exe_name("old_3_Aira.exe", EXE));
		assert!(is_old_exe_name("OLD_aira.EXE", EXE));
		assert!(!is_old_exe_name("old__Aira.exe", EXE));
		assert!(!is_old_exe_name("old_x_Aira.exe", EXE));
		assert!(!is_old_exe_name("new_Aira.exe", EXE));
		assert!(!is_old_exe_name("Aira.exe", EXE));
	}

	// ---- inspect, against real folders ----

	fn write(path: &Path, body: &str) {
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, body).unwrap();
	}

	fn install(base: &Path, body: &str) {
		write(&base.join(EXE), body);
		write(&base.join("icudtl.dat"), body);
		write(&base.join("locales").join("en-US.pak"), body);
		write(
			&base.join("resources").join("app").join("package.json"),
			body,
		);
		write(
			&base.join("resources").join("app").join("product.json"),
			body,
		);
	}

	#[test]
	fn inspect_reads_a_healthy_install() {
		let dir = tempdir().unwrap();
		install(dir.path(), "old");
		write(&dir.path().join("unins000.dat"), "u");
		let e = inspect(dir.path(), EXE).unwrap();
		assert!(e.root_exe && e.root_marker);
		assert_eq!(e.stage, Stage::Absent);
		assert_eq!(decide(&e).outcome, Outcome::NothingToDo);
	}

	#[test]
	fn inspect_finds_old_files_under_new_paths() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		install(root, "old");
		install(&root.join(STAGE_DIR), "new");
		fs::remove_file(root.join(EXE)).unwrap();
		fs::remove_file(root.join("icudtl.dat")).unwrap();

		let e = inspect(root, EXE).unwrap();
		assert!(!e.root_exe);
		assert_eq!(staged(true, true), e.stage);
		let mut overlap = e.overlap.clone();
		overlap.sort();
		assert_eq!(
			overlap,
			vec![
				Path::new("locales").join("en-US.pak"),
				Path::new("resources").join("app").join("package.json"),
				Path::new("resources").join("app").join("product.json"),
			]
		);
		assert!(e.conflicts.is_empty());
		assert_eq!(decide(&e).outcome, Outcome::Completed);
	}

	#[test]
	fn inspect_sees_a_type_conflict() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		write(
			&root.join("locales"),
			"a file where the new version has a folder",
		);
		install(&root.join(STAGE_DIR), "new");
		let e = inspect(root, EXE).unwrap();
		assert_eq!(e.conflicts, vec![PathBuf::from("locales")]);
		assert_eq!(decide(&e).outcome, Outcome::Refused);
	}

	#[test]
	fn inspect_ignores_husks_left_by_an_earlier_update() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		install(&root.join(STAGE_DIR), "new");
		write(
			&root
				.join("resources")
				.join("app")
				.join("main.js.deleting-12-0"),
			"husk",
		);
		write(&root.join(STAGE_DIR).join("x.deleting-12-1"), "husk");
		let e = inspect(root, EXE).unwrap();
		assert!(e.overlap.is_empty());
		assert!(e.conflicts.is_empty());
	}

	#[test]
	fn inspect_lists_preserved_names_in_the_stage() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		install(&root.join(STAGE_DIR), "new");
		write(
			&root.join(STAGE_DIR).join("tools").join("inno_updater.exe"),
			"x",
		);
		let e = inspect(root, EXE).unwrap();
		match e.stage {
			Stage::Present { preserved, .. } => assert_eq!(preserved, vec!["tools".to_string()]),
			other => panic!("unexpected {:?}", other),
		}
	}

	#[test]
	fn inspect_reads_the_versioned_names() {
		let dir = tempdir().unwrap();
		let root = dir.path();
		write(&root.join("new_Aira.exe"), "new");
		write(&root.join("old_Aira.exe"), "old");
		write(&root.join("old_2_Aira.exe"), "older");
		write(&root.join("old_notes.txt"), "not an exe");
		let e = inspect(root, EXE).unwrap();
		assert!(!e.root_exe);
		assert!(e.new_exe);
		assert_eq!(
			e.old_exes,
			vec!["old_2_Aira.exe".to_string(), "old_Aira.exe".to_string()]
		);
		assert_eq!(decide(&e).action, Action::PromoteNewExe);
	}

	#[test]
	fn inspect_rejects_a_missing_root() {
		let dir = tempdir().unwrap();
		assert!(inspect(&dir.path().join("nope"), EXE).is_err());
	}
}
