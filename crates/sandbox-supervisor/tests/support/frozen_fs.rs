//! Root-only fault injection on a new, exclusively owned loopback filesystem.
#![allow(clippy::unwrap_used)]
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

#[derive(Debug)]
pub(super) struct Filesystem {
    temp: tempfile::TempDir,
    mount: PathBuf,
}
impl Filesystem {
    pub(super) fn new() -> Self {
        assert_eq!(std::env::var("HUDSON_GUARDIAN_TEST_VM").as_deref(), Ok("1"));
        assert!(rustix::process::geteuid().is_root());
        let temp = tempfile::Builder::new().prefix("hf-").tempdir().unwrap();
        let backing = temp.path().join("disk.ext4");
        fs::File::create(&backing)
            .unwrap()
            .set_len(512 << 20)
            .unwrap();
        assert!(
            Command::new("/usr/sbin/mkfs.ext4")
                .args(["-q", "-F"])
                .arg(&backing)
                .status()
                .unwrap()
                .success()
        );
        let mount = temp.path().join("m");
        fs::create_dir(&mount).unwrap();
        assert!(
            Command::new("/usr/bin/mount")
                .args(["-o", "loop,nosuid"])
                .arg(&backing)
                .arg(&mount)
                .status()
                .unwrap()
                .success()
        );
        let owned = Self { temp, mount };
        assert_ne!(
            fs::metadata(owned.path()).unwrap().dev(),
            fs::metadata(owned.temp.path()).unwrap().dev(),
            "refuse to freeze a directory on the parent filesystem"
        );
        owned
    }
    pub(super) fn path(&self) -> &Path {
        &self.mount
    }
    pub(super) fn freeze(&self) -> Frozen<'_> {
        // Independent of the test process and its locks: if the test is killed,
        // this helper thaws only this owned filesystem after 30 seconds.
        let thaw = Command::new("/usr/bin/python3")
            .args(["-c", "import subprocess,sys,time; time.sleep(30); subprocess.run(['/usr/sbin/fsfreeze','-u',sys.argv[1]],check=False)"])
            .arg(&self.mount).env_clear().stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .spawn().unwrap();
        let guard = Frozen {
            owner: self,
            helper: thaw,
            frozen: true,
        };
        assert!(
            Command::new("/usr/sbin/fsfreeze")
                .arg("-f")
                .arg(&self.mount)
                .status()
                .unwrap()
                .success()
        );
        guard
    }
}
impl Drop for Filesystem {
    fn drop(&mut self) {
        let clean = Command::new("/usr/bin/umount")
            .arg(&self.mount)
            .status()
            .is_ok_and(|s| s.success());
        if !clean {
            self.temp.disable_cleanup(true);
            eprintln!(
                "retained owned loopback filesystem after unmount failure: {:?}",
                self.temp.path()
            );
        }
    }
}
#[derive(Debug)]
pub(super) struct Frozen<'a> {
    owner: &'a Filesystem,
    helper: Child,
    frozen: bool,
}
impl Frozen<'_> {
    pub(super) fn thaw(&mut self) {
        if self.frozen {
            assert!(
                Command::new("/usr/sbin/fsfreeze")
                    .arg("-u")
                    .arg(self.owner.path())
                    .status()
                    .unwrap()
                    .success(),
                "owned filesystem thaw failed"
            );
            self.frozen = false;
        }
    }
}
impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        if self.frozen {
            // Do not panic during unwind. Keep the independent helper alive if
            // the immediate thaw fails; it must not be cancelled in that case.
            if !Command::new("/usr/sbin/fsfreeze")
                .arg("-u")
                .arg(self.owner.path())
                .status()
                .is_ok_and(|s| s.success())
            {
                return;
            }
        }
        let _ = self.helper.kill();
        let _ = self.helper.wait();
    }
}
