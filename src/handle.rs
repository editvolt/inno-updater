/*-----------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See LICENSE in the project root for license information.
 *----------------------------------------------------------------------------------------*/

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{error, io, ptr};
use crate::strings::to_u16s;
use crate::util;
use windows_sys::Win32::Foundation::HANDLE;

static ASIDE_COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct FileHandle {
	handle: HANDLE,
	path: PathBuf,
}

impl FileHandle {
	pub fn new(path: &Path) -> Result<FileHandle, Box<dyn error::Error>> {
		use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
		use windows_sys::Win32::Storage::FileSystem::{
			CreateFileW, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
			FILE_SHARE_WRITE, OPEN_EXISTING,
		};

		unsafe {
			let path_wide = to_u16s(path.as_os_str());
			// The only thing this handle is ever used for is DELETE. Asking for it with
			// dwShareMode = 0 asks for much more than that: the open then fails unless NO
			// other process holds ANY handle on the file. An antivirus service keeping a
			// read handle on the main executable is enough, and that is not a state the
			// updater can wait out -- it is not our process and it does not have to let
			// go. Sharing the file costs nothing here and removes a whole class of update
			// failure that presents as "used by another process (os error 32)" after
			// every one of the application's own processes has already exited.
			let handle = CreateFileW(
				path_wide.as_ptr(),
				DELETE,
				FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
				ptr::null_mut(),
				OPEN_EXISTING,
				FILE_ATTRIBUTE_NORMAL,
				std::mem::zeroed(),
			);

			if handle == INVALID_HANDLE_VALUE {
				return Err(io::Error::last_os_error().into());
			}

			Ok(FileHandle {
				handle,
				path: path.to_path_buf(),
			})
		}
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Move the directory entry out of the way, so the NAME is free immediately.
	///
	/// Marking a file for deletion only schedules it: the entry survives until the LAST
	/// handle on the file closes, and the updater does not own all of them -- a virus
	/// scanner can be holding one, and does not have to let go on our schedule. Until it
	/// does, the name stays behind in a delete-pending state where every open of it
	/// fails, including the rename that moves the new version into that exact name.
	///
	/// Renaming through the handle we already hold sidesteps that. The name is free the
	/// moment the call returns no matter who else has the file open, and the renamed
	/// entry still carries the delete disposition, so it disappears by itself as soon as
	/// the last holder lets go.
	///
	/// Measured, not assumed: `FileDispositionInfoEx` with `POSIX_SEMANTICS` was tried
	/// first and does NOT free the name while another handle is open -- the call reports
	/// success and the entry stays. A rename does free it.
	pub fn rename_aside(&self) -> Result<(), Box<dyn error::Error>> {
		use std::mem;
		use windows_sys::Win32::Storage::FileSystem::{
			FileRenameInfo, SetFileInformationByHandle, FILE_RENAME_INFO,
		};

		let file_name = self
			.path
			.file_name()
			.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Could not get file name"))?
			.to_string_lossy()
			.into_owned();
		let directory = self
			.path
			.parent()
			.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Could not get parent path"))?;

		// Distinct per process and per file, so two of these can never collide and a
		// leftover is recognizable in a directory listing. Leftovers only happen when a
		// holder never lets go, and the next update sweeps them: it deletes everything
		// in the old installation anyway.
		let unique = ASIDE_COUNTER.fetch_add(1, Ordering::Relaxed);
		let target = directory.join(format!(
			"{}.deleting-{}-{}",
			file_name,
			std::process::id(),
			unique
		));

		// FILE_RENAME_INFO is variable length: the struct, then the name in place of its
		// one-element FileName array. Building it through the struct rather than by hand
		// keeps the field offsets right on both i686 and x64, where HANDLE differs in
		// size and alignment.
		let target_wide = to_u16s(target.as_os_str());
		let name = &target_wide[..target_wide.len() - 1]; // the length excludes the terminator
		let name_bytes = name.len() * mem::size_of::<u16>();
		let mut buffer = vec![0u8; mem::size_of::<FILE_RENAME_INFO>() + name_bytes];

		unsafe {
			let info = buffer.as_mut_ptr() as *mut FILE_RENAME_INFO;
			(*info).Anonymous.ReplaceIfExists = 0;
			(*info).RootDirectory = 0 as HANDLE;
			(*info).FileNameLength = name_bytes as u32;
			ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());

			let result = SetFileInformationByHandle(
				self.handle,
				FileRenameInfo,
				buffer.as_ptr() as *const c_void,
				buffer.len() as u32,
			);

			if result == 0 {
				return Err(io::Error::new(
					io::ErrorKind::Other,
					format!(
						"Failed to move {:?} aside: {}",
						self.path,
						util::get_last_error_message()?
					),
				)
				.into());
			}
		}

		Ok(())
	}

	pub fn mark_for_deletion(&self) -> Result<(), Box<dyn error::Error>> {
		use std::mem;
		use windows_sys::Win32::Foundation::BOOLEAN;
		use windows_sys::Win32::Storage::FileSystem::{
			FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
		};

		unsafe {
			let mut info = FILE_DISPOSITION_INFO {
				DeleteFile: 1 as BOOLEAN,
			};
			let result = SetFileInformationByHandle(
				self.handle,
				FileDispositionInfo,
				&mut info as *mut _ as *mut c_void,
				mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
			);

			// SetFileInformationByHandle returns BOOL, whose failure value is 0 and never
			// negative, so the `is_negative()` check this replaces could not fire: a
			// failed disposition was reported to the caller as success.
			if result == 0 {
				return Err(io::Error::new(
					io::ErrorKind::Other,
					format!(
						"Failed to mark file for deletion: {}",
						util::get_last_error_message()?
					),
				)
				.into());
			}
		}

		Ok(())
	}

	pub fn close(&self) -> Result<(), Box<dyn error::Error>> {
		use windows_sys::Win32::Foundation::CloseHandle;

		unsafe {
			if CloseHandle(self.handle) == 0 {
				return Err(io::Error::new(
					io::ErrorKind::Other,
					format!(
						"Failed to close file handle: {}",
						util::get_last_error_message()?
					),
				)
				.into());
			}
		}

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	/// Regression: every background update failed with "The process cannot access the
	/// file because it is being used by another process. (os error 32)" on the main
	/// executable, minutes after all of the application's own processes had exited. The
	/// holder was an antivirus service, and an exclusive open cannot wait that out.
	///
	/// No antivirus is needed to reproduce it: before the fix ANY second handle on the
	/// file was enough. `fs::File::open` shares read, write and delete, which is how a
	/// well-behaved scanner holds a file.
	#[test]
	fn replaces_a_file_another_process_holds_open() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("held.exe");
		fs::write(&path, b"old").unwrap();

		let scanner = fs::File::open(&path).unwrap();

		let handle = FileHandle::new(&path).expect("opening must not require exclusive access");
		handle.rename_aside().expect("the entry must move aside");
		handle
			.mark_for_deletion()
			.expect("marking for deletion must work while another handle is open");
		handle.close().unwrap();

		// The name has to be free NOW, not when the scanner lets go: moving the new
		// version into that exact name is the updater's very next step.
		assert!(!path.exists(), "the name must be free while the file is held");
		fs::write(&path, b"new").expect("the new version must take the freed name");
		assert_eq!(fs::read(&path).unwrap(), b"new");

		drop(scanner);
	}

	/// The whole directory fallback rests on this being true: a directory can be RENAMED
	/// while a file inside it is still open, even though it cannot be REMOVED.
	///
	/// This is what saves an update when a scanner is holding something. By the time the
	/// old installation's directories are removed, every file has already been marked for
	/// deletion -- so a failure there leaves neither the old version nor the new one. That
	/// is not hypothetical: a live 0.4.10 -> 0.4.11 update failed exactly this way, with a
	/// scanner holding three executables, and left the machine with no application.
	#[test]
	fn a_directory_renames_while_a_file_inside_it_is_held() {
		let dir = tempfile::tempdir().unwrap();
		let sub = dir.path().join("resources");
		fs::create_dir(&sub).unwrap();
		let inner = sub.join("held.exe");
		fs::write(&inner, b"payload").unwrap();

		// A scanner's handle: shares read, write and delete, like std does.
		let scanner = fs::File::open(&inner).unwrap();

		let aside = dir.path().join("resources.deleting-1234");
		fs::rename(&sub, &aside)
			.expect("a directory must be renameable while a file inside it is open");

		assert!(!sub.exists(), "the original name must be free for the new version");
		assert!(
			dir.path().join("resources").parent().is_some(),
			"and the parent must still be usable"
		);
		// The name is free, so the update can put the new directory here.
		fs::create_dir(&sub).expect("the freed name must be reusable immediately");

		drop(scanner);
	}

	/// The ordinary case: nobody else has the file, so it goes away completely.
	#[test]
	fn deletes_a_file_nobody_holds_open() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("free.exe");
		fs::write(&path, b"payload").unwrap();

		let handle = FileHandle::new(&path).unwrap();
		handle.rename_aside().unwrap();
		handle.mark_for_deletion().unwrap();
		handle.close().unwrap();

		assert!(!path.exists(), "the file must be gone");
		assert_eq!(
			fs::read_dir(dir.path()).unwrap().count(),
			0,
			"and must leave nothing behind"
		);
	}
}
