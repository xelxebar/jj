// Copyright 2022 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::path::Path;

use crate::common::TestEnvironment;

pub mod common;

#[test]
fn test_commit_with_description_from_cli() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let workspace_path = test_env.env_root().join("repo");

    // Description applies to the current working-copy (not the new one)
    test_env.jj_cmd_success(&workspace_path, &["commit", "-m=first"]);
    insta::assert_snapshot!(get_log_output(&test_env, &workspace_path), @r###"
    @  b88fb4e51bdd
    ●  69542c1984c1 first
    ●  000000000000
    "###);
}

#[test]
fn test_commit_with_editor() {
    let mut test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let workspace_path = test_env.env_root().join("repo");

    // Check that the text file gets initialized with the current description and
    // set a new one
    test_env.jj_cmd_success(&workspace_path, &["describe", "-m=initial"]);
    let edit_script = test_env.set_up_fake_editor();
    std::fs::write(&edit_script, ["dump editor0", "write\nmodified"].join("\0")).unwrap();
    test_env.jj_cmd_success(&workspace_path, &["commit"]);
    insta::assert_snapshot!(get_log_output(&test_env, &workspace_path), @r###"
    @  3df78bc2b9b5
    ●  30a8c2b3d6eb modified
    ●  000000000000
    "###);
    insta::assert_snapshot!(
        std::fs::read_to_string(test_env.env_root().join("editor0")).unwrap(), @r###"
    initial

    JJ: Lines starting with "JJ: " (like this one) will be removed.
    "###);

    // Check that the editor content includes diff summary
    std::fs::write(workspace_path.join("file1"), "foo\n").unwrap();
    std::fs::write(workspace_path.join("file2"), "foo\n").unwrap();
    test_env.jj_cmd_success(&workspace_path, &["describe", "-m=add files"]);
    std::fs::write(&edit_script, "dump editor1").unwrap();
    test_env.jj_cmd_success(&workspace_path, &["commit"]);
    insta::assert_snapshot!(
        std::fs::read_to_string(test_env.env_root().join("editor1")).unwrap(), @r###"
    add files

    JJ: This commit contains the following changes:
    JJ:     A file1
    JJ:     A file2

    JJ: Lines starting with "JJ: " (like this one) will be removed.
    "###);
}

#[test]
fn test_commit_without_working_copy() {
    let test_env = TestEnvironment::default();
    test_env.jj_cmd_success(test_env.env_root(), &["init", "repo", "--git"]);
    let workspace_path = test_env.env_root().join("repo");

    test_env.jj_cmd_success(&workspace_path, &["workspace", "forget"]);
    let stderr = test_env.jj_cmd_failure(&workspace_path, &["commit", "-m=first"]);
    insta::assert_snapshot!(stderr, @r###"
    Error: This command requires a working copy
    "###);
}

fn get_log_output(test_env: &TestEnvironment, cwd: &Path) -> String {
    let template = r#"commit_id.short() ++ " " ++ description"#;
    test_env.jj_cmd_success(cwd, &["log", "-T", template])
}
