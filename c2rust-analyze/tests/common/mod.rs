use std::{
    collections::HashSet,
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    process::Command,
};

use c2rust_build_paths::find_llvm_config;

#[derive(Default)]
pub struct Analyze {
    path: PathBuf,
}

impl Analyze {
    pub fn resolve() -> Self {
        let current_exe = env::current_exe().unwrap();
        let bin_deps_dir = current_exe.parent().unwrap();
        let bin_dir = bin_deps_dir.parent().unwrap();
        let path = bin_dir.join(env!("CARGO_PKG_NAME"));
        Self { path }
    }

    fn run_(&self, rs_path: &Path) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let lib_dir = Path::new(env!("C2RUST_TARGET_LIB_DIR"));

        let rs_path = dir.join(rs_path); // allow relative paths, or override with an absolute path

        let directives = fs::read_to_string(&rs_path)
            .unwrap()
            .split('\n')
            .flat_map(|line| {
                line.trim()
                    .strip_prefix("//!")
                    .unwrap_or_default()
                    .split(',')
                    .map(|directive| directive.trim())
            })
            .map(String::from)
            .collect::<HashSet<_>>();

        let output_path = {
            let mut file_name = rs_path.file_name().unwrap().to_owned();
            file_name.push(".analysis.txt");
            rs_path.with_file_name(file_name)
        };
        let output_stdout = File::create(&output_path).unwrap();
        let output_stderr = File::try_clone(&output_stdout).unwrap();

        let mut cmd = Command::new(&self.path);
        cmd.arg(&rs_path)
            .arg("-L")
            .arg(lib_dir)
            .arg("--crate-type")
            .arg("rlib")
            .stdout(output_stdout)
            .stderr(output_stderr);
        let status = cmd.status().unwrap();
        if !status.success() && !directives.contains("allow_crash") {
            let message = format!(
                "c2rust-analyze failed with status {status}:\n> {cmd:?} > {output_path:?} 2>&1\n"
            );
            let output = fs::read_to_string(&output_path).unwrap();
            let max_len = 80 * 300; // don't print a ton
            let output = if output.len() > max_len {
                let len_truncated = output.len() - max_len;
                let (truncated, rest) = output.split_at(len_truncated);
                let num_lines_truncated = truncated.split('\n').count();
                let truncated_msg = format!(
                    "truncated the first {len_truncated} bytes ({num_lines_truncated} lines)"
                );
                format!("[{truncated_msg}] ...\n{rest}\n[note: {truncated_msg}]")
            } else {
                output
            };
            panic!("\n{message}\n{output}\n{message}");
        }
        output_path
    }

    pub fn run(&self, rs_path: impl AsRef<Path>) -> PathBuf {
        self.run_(rs_path.as_ref())
    }
}

pub struct FileCheck {
    path: PathBuf,
}

impl FileCheck {
    pub fn resolve() -> Self {
        let path = env::var_os("FILECHECK")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let llvm_config = find_llvm_config().expect("llvm-config not found");
                let output = Command::new(llvm_config)
                    .args(&["--bindir"])
                    .output()
                    .ok()
                    .filter(|output| output.status.success())
                    .expect("llvm-config error");
                let bin_dir =
                    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim().to_owned());
                bin_dir.join("FileCheck")
            });
        Self { path }
    }

    fn run_(&self, path: &Path, input: &Path) {
        let mut cmd = Command::new(&self.path);
        let input_file = File::open(input).unwrap();
        cmd.arg(path).stdin(input_file);
        let status = cmd.status().unwrap();
        assert!(
            status.success(),
            "\nFileCheck failed with status {status}:\n> {cmd:?} > {input:?}\n",
        );
    }

    pub fn run(&self, path: impl AsRef<Path>, input: impl AsRef<Path>) {
        self.run_(path.as_ref(), input.as_ref())
    }
}
