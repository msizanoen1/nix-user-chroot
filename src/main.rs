use nix::mount::{mount, umount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::unistd;
use std::env;
use std::fs;
use std::fs::Permissions;
use std::io;
use std::io::prelude::*;
use std::os::unix::fs::symlink;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process;
use std::string::String;
use tempfile::TempDir;

struct WrapUmount {
    dir: TempDir,
    defused: bool,
}

impl Drop for WrapUmount {
    fn drop(&mut self) {
        if !self.defused {
            umount(self.dir.path()).unwrap();
        }
    }
}

impl std::ops::Deref for WrapUmount {
    type Target = TempDir;

    fn deref(&self) -> &Self::Target {
        &self.dir
    }
}

impl WrapUmount {
    fn defuse(&mut self) {
        self.defused = true;
    }

    fn new(dir: TempDir) -> Self {
        Self {
            dir,
            defused: false,
        }
    }
}

const NONE: Option<&'static [u8]> = None;

fn bind_mount(source: &Path, dest: &Path) {
    if let Err(e) = mount(
        Some(source),
        dest,
        Some("none"),
        MsFlags::MS_BIND | MsFlags::MS_REC,
        NONE,
    ) {
        eprintln!(
            "failed to bind mount {} to {}: {}",
            source.display(),
            dest.display(),
            e
        );
    }
}

fn bind_mount_directory(entry: &fs::DirEntry) {
    let mountpoint = PathBuf::from("/").join(entry.file_name());
    if let Err(e) = fs::create_dir(&mountpoint) {
        if e.kind() != io::ErrorKind::AlreadyExists {
            let e2: io::Result<()> = Err(e);
            e2.unwrap_or_else(|_| panic!("failed to create {}", &mountpoint.display()));
        }
    }

    bind_mount(&entry.path(), &mountpoint)
}

fn bind_mount_file(entry: &fs::DirEntry) {
    let mountpoint = PathBuf::from("/").join(entry.file_name());
    fs::File::create(&mountpoint)
        .unwrap_or_else(|_| panic!("failed to create {}", &mountpoint.display()));

    bind_mount(&entry.path(), &mountpoint)
}

fn mirror_symlink(entry: &fs::DirEntry) {
    let path = entry.path();
    let target = fs::read_link(&path)
        .unwrap_or_else(|_| panic!("failed to resolve symlink {}", &path.display()));
    let link_path = PathBuf::from("/").join(entry.file_name());
    symlink(&target, &link_path).unwrap_or_else(|_| {
        panic!(
            "failed to create symlink {} -> {}",
            &link_path.display(),
            &target.display()
        )
    });
}

fn bind_mount_direntry(entry: io::Result<fs::DirEntry>) {
    let entry = entry.expect("error while listing from /nix directory");
    // do not bind mount an existing nix installation
    if entry.file_name() == PathBuf::from("nix") {
        return;
    }
    let path = entry.path();
    let stat = entry
        .metadata()
        .unwrap_or_else(|_| panic!("cannot get stat of {}", path.display()));
    if stat.is_dir() {
        bind_mount_directory(&entry);
    } else if stat.is_file() {
        bind_mount_file(&entry);
    } else if stat.file_type().is_symlink() {
        mirror_symlink(&entry);
    }
}

fn run_chroot(nixdir: &Path, cmd: &str, args: &[String]) {
    let tempdir = TempDir::new().expect("failed to create temporary directory for mount point");
    let mut tempdir = WrapUmount::new(tempdir);
    let rootdir = PathBuf::from(tempdir.path());

    let cwd = env::current_dir().expect("cannot get current working directory");

    let uid = unistd::getuid();
    let gid = unistd::getgid();
    // fixes issue #1 where writing to /proc/self/gid_map fails
    // see user_namespaces(7) for more documentation
    unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUSER).expect("unshare failed");
    if let Ok(mut file) = fs::File::create("/proc/self/setgroups") {
        let _ = file.write_all(b"deny");
    }

    let mut uid_map =
        fs::File::create("/proc/self/uid_map").expect("failed to open /proc/self/uid_map");
    uid_map
        .write_all(format!("{} {} 1", uid, uid).as_bytes())
        .expect("failed to write new uid mapping to /proc/self/uid_map");

    let mut gid_map =
        fs::File::create("/proc/self/gid_map").expect("failed to open /proc/self/gid_map");
    gid_map
        .write_all(format!("{} {} 1", gid, gid).as_bytes())
        .expect("failed to write new gid mapping to /proc/self/gid_map");

    // prepare pivot_root call:
    // rootdir must be a mount point

    mount(
        Some("/"),
        "/",
        Some("none"),
        MsFlags::MS_BIND | MsFlags::MS_REC,
        NONE,
    )
    .unwrap();

    mount(
        Some("none"),
        "/",
        Some("none"),
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        NONE,
    )
    .unwrap();

    mount(
        Some("none"),
        &rootdir,
        Some("tmpfs"),
        MsFlags::empty(),
        NONE,
    )
    .unwrap();

    fs::set_permissions(&rootdir, Permissions::from_mode(0o755)).unwrap();

    // create the mount point for the old root
    // The old root cannot be unmounted/removed after pivot_root, the only way to
    // keep / clean is to hide the directory with another mountpoint. Therefore
    // we pivot the old root to /nix. This is somewhat confusing, though.
    let nix_mountpoint = rootdir.join("nix");
    fs::create_dir(&nix_mountpoint).unwrap();

    unistd::pivot_root(&rootdir, &nix_mountpoint).unwrap();
    tempdir.defuse();
    env::set_current_dir("/").expect("cannot change directory to /");

    // bind mount all / stuff into rootdir
    // the orginal content of / now available under /nix
    let nix_root = PathBuf::from("/nix");
    let dir = fs::read_dir(&nix_root).expect("failed to list /nix directory");
    for entry in dir {
        bind_mount_direntry(entry);
    }
    drop(tempdir);
    if PathBuf::from("/nix/nix/store").exists() {
        let tmp = WrapUmount::new(TempDir::new().unwrap());
        mount(
            Some("/nix/nix"),
            "/nix",
            Some("none"),
            MsFlags::MS_BIND | MsFlags::MS_REC,
            NONE,
        )
        .unwrap();
        mount(
            Some("/nix/store"),
            tmp.path(),
            Some("none"),
            MsFlags::MS_BIND | MsFlags::MS_REC,
            NONE,
        )
        .unwrap();
        mount(
            Some("none"),
            "/nix/store",
            Some("tmpfs"),
            MsFlags::empty(),
            Some("mode=0755"),
        )
        .unwrap();
        let sroot = PathBuf::from("/nix/store");
        for entry in fs::read_dir(&nixdir).unwrap() {
            let entry = entry.unwrap();
            let stat = entry.metadata().unwrap();
            let name = entry.file_name();
            let path = sroot.join(&name);
            let store_path = entry.path();
            if stat.is_dir() {
                fs::create_dir(&path).unwrap();
            } else if stat.is_file() {
                fs::File::create(&path).unwrap();
            } else if stat.file_type().is_symlink() {
                let target = fs::read_link(&store_path).unwrap();
                symlink(&target, &path).unwrap();
            }
            if stat.is_dir() || stat.is_file() {
                mount(
                    Some(&store_path),
                    &path,
                    Some("none"),
                    MsFlags::MS_BIND | MsFlags::MS_REC,
                    NONE,
                )
                .unwrap();
            }
        }
        if let Ok(iter) = fs::read_dir(tmp.path()) {
            for entry in iter {
                let entry = entry.unwrap();
                let name = entry.file_name();
                let path = sroot.join(&name);
                if path.exists() {
                    continue;
                }
                let stat = entry.metadata().unwrap();
                let store_path = entry.path();
                if stat.is_dir() {
                    fs::create_dir(&path).unwrap();
                } else if stat.is_file() {
                    fs::File::create(&path).unwrap();
                } else if stat.file_type().is_symlink() {
                    let target = fs::read_link(&store_path).unwrap();
                    symlink(&target, &path).unwrap();
                }
                if stat.is_dir() || stat.is_file() {
                    mount(
                        Some(&store_path),
                        &path,
                        Some("none"),
                        MsFlags::MS_BIND | MsFlags::MS_REC,
                        NONE,
                    )
                    .unwrap();
                }
            }
        }
        mount(
            Some("none"),
            "/nix/store",
            Some("none"),
            MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
            NONE,
        )
        .unwrap();
    } else {
        mount(
            Some("none"),
            "/nix",
            Some("tmpfs"),
            MsFlags::empty(),
            Some("mode=0755"),
        )
        .unwrap();
        fs::create_dir_all("/nix/store").unwrap();
        mount(
            Some(nixdir),
            "/nix/store",
            Some("none"),
            MsFlags::MS_BIND | MsFlags::MS_REC,
            NONE,
        )
        .unwrap();
    }

    // restore cwd
    env::set_current_dir(&cwd)
        .unwrap_or_else(|_| panic!("cannot restore working directory {}", cwd.display()));

    let err = process::Command::new(cmd).args(args).exec();

    eprintln!("failed to execute {}: {}", &cmd, err);
    process::exit(1);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <nixpath> <command>\n", args[0]);
        process::exit(1);
    }
    let nixdir = fs::canonicalize(&args[1])
        .unwrap_or_else(|_| panic!("failed to resolve nix directory {}", &args[1]));

    run_chroot(&nixdir, &args[2], &args[3..]);
}
