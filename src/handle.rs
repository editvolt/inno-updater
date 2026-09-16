/*-----------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See LICENSE in the project root for license information.
 *----------------------------------------------------------------------------------------*/

use std::ffi::c_void;
use std::path::Path;
use std::{error, io, ptr};
use crate::strings::to_u16s;
use crate::util;
use windows_sys::Win32::Foundation::HANDLE;

const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x0000_0001;
const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x0000_0002;
const FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE: u32 = 0x0000_0010;

pub struct FileHandle(HANDLE);

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
			// dwShareMode = 0 asks for much more than that: it fails unless NO other
			// process holds ANY handle on the file. An antivirus service keeping a
			// read handle on the main executable is enough, and that is not a state the
			// updater can wait out -- it is not our process and it does not have to let
			// go. Sharing the file costs nothing here and removes a whole class of
			// update failure that presents as "used by another process (os error 32)"
			// after every one of the application's own processes has already exited.
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

			Ok(FileHandle(handle))
		}
	}

	pub fn mark_for_deletion(&self) -> Result<(), Box<dyn error::Error>> {
		// Unlink the NAME now, rather than when the last handle closes.
		//
		// The legacy disposition only schedules the delete: the directory entry stays
		// behind in a delete-pending state until every handle on the file is closed, and
		// the caller does not own all of them -- a scanner can be holding one. Every open
		// of a delete-pending name fails, so the entry blocks the very rename the updater
		// performs next to move the new version into place.
		//
		// POSIX semantics remove the entry immediately and let the remaining handles keep
		// reading the now-nameless file until they close on their own. That is exactly
		// what this code path wants. It needs Windows 10 1709+ on NTFS, so an older system
		// or another filesystem falls back to the legacy behaviour, which is no worse than
		// what shipped before.
		match self.set_disposition_posix() {
			Ok(()) => Ok(()),
			Err(_) => self.set_disposition_legacy(),
		}
	}

	fn set_disposition_posix(&self) -> Result<(), Box<dyn error::Error>> {
		self.set_disposition_ex(
			FILE_DISPOSITION_FLAG_DELETE
				| FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
				| FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
		)
	}

	fn set_disposition_ex(&self, flags: u32) -> Result<(), Box<dyn error::Error>> {
		use std::mem;
		use windows_sys::Win32::Storage::FileSystem::{
			FileDispositionInfoEx, SetFileInformationByHandle,
		};

		// windows-sys 0.42 exposes the info class but not the struct.
		#[repr(C)]
		struct FileDispositionInfoExData {
			flags: u32,
		}

		unsafe {
			let mut info = FileDispositionInfoExData { flags };
			let result = SetFileInformationByHandle(
				self.0,
				FileDispositionInfoEx,
				&mut info as *mut _ as *mut c_void,
				mem::size_of::<FileDispositionInfoExData>() as u32,
			);

			if result == 0 {
				return Err(io::Error::new(
					io::ErrorKind::Other,
					format!(
						"Failed to unlink file: {}",
						util::get_last_error_message()?
					),
				)
				.into());
			}
		}

		Ok(())
	}

	fn set_disposition_legacy(&self) -> Result<(), Box<dyn error::Error>> {
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
				self.0,
				FileDispositionInfo,
				&mut info as *mut _ as *mut c_void,
				mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
			);

			// SetFileInformationByHandle returns BOOL, whose failure value is 0 and
			// never negative, so the `is_negative()` check this replaces could not
			// fire: a failed disposition was reported to the caller as success.
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
			if CloseHandle(self.0).is_negative() {
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
	/// No antivirus is needed to reproduce it: before the fix, ANY second handle on the
	/// file was enough. `fs::File::open` shares read, write and delete, which is how a
	/// well-behaved scanner holds a file.
	#[test]
	fn deletes_a_file_another_process_holds_open() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("held.exe");
		fs::write(&path, b"payload").unwrap();

		let scanner = fs::File::open(&path).unwrap();

		let handle = FileHandle::new(&path).expect("opening must not require exclusive access");

		// Report which combination the OS accepts, so a failure here says why rather
		// than only that the name survived.
		let mut report = String::new();
		for (label, flags) in [
			("DELETE", FILE_DISPOSITION_FLAG_DELETE),
			(
				"DELETE|POSIX",
				FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
			),
			(
				"DELETE|POSIX|IGNORE_READONLY",
				FILE_DISPOSITION_FLAG_DELETE
					| FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
					| FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
			),
		] {
			match handle.set_disposition_ex(flags) {
				Ok(()) => report.push_str(&format!("  {label}: ok, exists={}\n", path.exists())),
				Err(err) => report.push_str(&format!("  {label}: {err}\n")),
			}
		}

		handle
			.mark_for_deletion()
			.expect("marking for deletion must work while another handle is open");

		// The name has to be gone NOW, not when the scanner lets go: the updater renames
		// the new version onto this path as its very next step, and a delete-pending
		// entry would fail that rename.
		assert!(
			!path.exists(),
			"the name must be unlinked while another handle is still open\n{report}"
		);

		handle.close().unwrap();
		drop(scanner);
	}

	#[test]
	fn deletes_a_file_nobody_holds_open() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("free.exe");
		fs::write(&path, b"payload").unwrap();

		let handle = FileHandle::new(&path).unwrap();
		handle.mark_for_deletion().unwrap();
		handle.close().unwrap();

		assert!(!path.exists(), "the file must be gone");
	}
}
