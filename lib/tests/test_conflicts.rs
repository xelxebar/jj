// Copyright 2021 The Jujutsu Authors
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

use jujutsu_lib::backend::{Conflict, ConflictTerm, FileId, TreeValue};
use jujutsu_lib::conflicts::{materialize_conflict, parse_conflict, update_conflict_from_content};
use jujutsu_lib::repo::Repo;
use jujutsu_lib::repo_path::RepoPath;
use jujutsu_lib::store::Store;
use testutils::TestRepo;

fn file_conflict_term(file_id: &FileId) -> ConflictTerm {
    ConflictTerm {
        value: TreeValue::File {
            id: file_id.clone(),
            executable: false,
        },
    }
}

#[test]
fn test_materialize_conflict_basic() {
    let test_repo = TestRepo::init(false);
    let store = test_repo.repo.store();

    let path = RepoPath::from_internal_string("file");
    let base_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2
line 3
line 4
line 5
",
    );
    let left_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2
left 3.1
left 3.2
left 3.3
line 4
line 5
",
    );
    let right_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2
right 3.1
line 4
line 5
",
    );

    // The left side should come first. The diff should be use the smaller (right)
    // side, and the left side should be a snapshot.
    let mut conflict = Conflict {
        removes: vec![file_conflict_term(&base_id)],
        adds: vec![file_conflict_term(&left_id), file_conflict_term(&right_id)],
    };
    insta::assert_snapshot!(
        &materialize_conflict_string(store, &path, &conflict),
        @r###"
    line 1
    line 2
    <<<<<<<
    +++++++
    left 3.1
    left 3.2
    left 3.3
    %%%%%%%
    -line 3
    +right 3.1
    >>>>>>>
    line 4
    line 5
    "###
    );
    // Swap the positive terms in the conflict. The diff should still use the right
    // side, but now the right side should come first.
    conflict.adds.reverse();
    insta::assert_snapshot!(
        &materialize_conflict_string(store, &path, &conflict),
        @r###"
    line 1
    line 2
    <<<<<<<
    %%%%%%%
    -line 3
    +right 3.1
    +++++++
    left 3.1
    left 3.2
    left 3.3
    >>>>>>>
    line 4
    line 5
    "###
    );
}

#[test]
fn test_materialize_conflict_multi_rebase_conflicts() {
    let test_repo = TestRepo::init(false);
    let store = test_repo.repo.store();

    // Create changes (a, b, c) on top of the base, and linearize them.
    let path = RepoPath::from_internal_string("file");
    let base_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2 base
line 3
",
    );
    let a_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2 a.1
line 2 a.2
line 2 a.3
line 3
",
    );
    let b_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2 b.1
line 2 b.2
line 3
",
    );
    let c_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2 c.1
line 3
",
    );

    // The order of (a, b, c) should be preserved. For all cases, the "a" side
    // should be a snapshot.
    let conflict = Conflict {
        removes: vec![file_conflict_term(&base_id), file_conflict_term(&base_id)],
        adds: vec![
            file_conflict_term(&a_id),
            file_conflict_term(&b_id),
            file_conflict_term(&c_id),
        ],
    };
    insta::assert_snapshot!(
        &materialize_conflict_string(store, &path, &conflict),
        @r###"
    line 1
    <<<<<<<
    +++++++
    line 2 a.1
    line 2 a.2
    line 2 a.3
    %%%%%%%
    -line 2 base
    +line 2 b.1
    +line 2 b.2
    %%%%%%%
    -line 2 base
    +line 2 c.1
    >>>>>>>
    line 3
    "###
    );
    let conflict = Conflict {
        removes: vec![file_conflict_term(&base_id), file_conflict_term(&base_id)],
        adds: vec![
            file_conflict_term(&c_id),
            file_conflict_term(&b_id),
            file_conflict_term(&a_id),
        ],
    };
    insta::assert_snapshot!(
        &materialize_conflict_string(store, &path, &conflict),
        @r###"
    line 1
    <<<<<<<
    %%%%%%%
    -line 2 base
    +line 2 c.1
    %%%%%%%
    -line 2 base
    +line 2 b.1
    +line 2 b.2
    +++++++
    line 2 a.1
    line 2 a.2
    line 2 a.3
    >>>>>>>
    line 3
    "###
    );
    let conflict = Conflict {
        removes: vec![file_conflict_term(&base_id), file_conflict_term(&base_id)],
        adds: vec![
            file_conflict_term(&c_id),
            file_conflict_term(&a_id),
            file_conflict_term(&b_id),
        ],
    };
    insta::assert_snapshot!(
        &materialize_conflict_string(store, &path, &conflict),
        @r###"
    line 1
    <<<<<<<
    %%%%%%%
    -line 2 base
    +line 2 c.1
    +++++++
    line 2 a.1
    line 2 a.2
    line 2 a.3
    %%%%%%%
    -line 2 base
    +line 2 b.1
    +line 2 b.2
    >>>>>>>
    line 3
    "###
    );
}

#[test]
fn test_materialize_parse_roundtrip() {
    let test_repo = TestRepo::init(false);
    let store = test_repo.repo.store();

    let path = RepoPath::from_internal_string("file");
    let base_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2
line 3
line 4
line 5
",
    );
    let left_id = testutils::write_file(
        store,
        &path,
        "line 1 left
line 2 left
line 3
line 4
line 5 left
",
    );
    let right_id = testutils::write_file(
        store,
        &path,
        "line 1 right
line 2
line 3
line 4 right
line 5 right
",
    );

    let conflict = Conflict {
        removes: vec![file_conflict_term(&base_id)],
        adds: vec![file_conflict_term(&left_id), file_conflict_term(&right_id)],
    };
    let mut result: Vec<u8> = vec![];
    materialize_conflict(store, &path, &conflict, &mut result).unwrap();
    insta::assert_snapshot!(
        String::from_utf8(result.clone()).unwrap(),
        @r###"
    <<<<<<<
    +++++++
    line 1 left
    line 2 left
    %%%%%%%
    -line 1
    +line 1 right
     line 2
    >>>>>>>
    line 3
    <<<<<<<
    %%%%%%%
     line 4
    -line 5
    +line 5 left
    +++++++
    line 4 right
    line 5 right
    >>>>>>>
    "###
    );

    // The first add should always be from the left side
    insta::assert_debug_snapshot!(
        parse_conflict(&result, conflict.removes.len(), conflict.adds.len()),
        @r###"
    Some(
        [
            Conflict {
                removes: [
                    "line 1\nline 2\n",
                ],
                adds: [
                    "line 1 left\nline 2 left\n",
                    "line 1 right\nline 2\n",
                ],
            },
            Resolved(
                "line 3\n",
            ),
            Conflict {
                removes: [
                    "line 4\nline 5\n",
                ],
                adds: [
                    "line 4\nline 5 left\n",
                    "line 4 right\nline 5 right\n",
                ],
            },
        ],
    )
    "###);
}

#[test]
fn test_materialize_conflict_modify_delete() {
    let test_repo = TestRepo::init(false);
    let store = test_repo.repo.store();

    let path = RepoPath::from_internal_string("file");
    let base_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2
line 3
line 4
line 5
",
    );
    let modified_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2
modified
line 4
line 5
",
    );
    let deleted_id = testutils::write_file(
        store,
        &path,
        "line 1
line 2
line 4
line 5
",
    );

    // left modifies a line, right deletes the same line.
    let conflict = Conflict {
        removes: vec![file_conflict_term(&base_id)],
        adds: vec![
            file_conflict_term(&modified_id),
            file_conflict_term(&deleted_id),
        ],
    };
    insta::assert_snapshot!(&materialize_conflict_string(store, &path, &conflict), @r###"
    line 1
    line 2
    <<<<<<<
    +++++++
    modified
    %%%%%%%
    -line 3
    >>>>>>>
    line 4
    line 5
    "###
    );

    // right modifies a line, left deletes the same line.
    let conflict = Conflict {
        removes: vec![file_conflict_term(&base_id)],
        adds: vec![
            file_conflict_term(&deleted_id),
            file_conflict_term(&modified_id),
        ],
    };
    insta::assert_snapshot!(&materialize_conflict_string(store, &path, &conflict), @r###"
    line 1
    line 2
    <<<<<<<
    %%%%%%%
    -line 3
    +++++++
    modified
    >>>>>>>
    line 4
    line 5
    "###
    );

    // modify/delete conflict at the file level
    let conflict = Conflict {
        removes: vec![file_conflict_term(&base_id)],
        adds: vec![file_conflict_term(&modified_id)],
    };
    insta::assert_snapshot!(&materialize_conflict_string(store, &path, &conflict), @r###"
    <<<<<<<
    %%%%%%%
     line 1
     line 2
    -line 3
    +modified
     line 4
     line 5
    +++++++
    >>>>>>>
    "###
    );
}

#[test]
fn test_parse_conflict_resolved() {
    assert_eq!(
        parse_conflict(
            b"line 1
line 2
line 3
line 4
line 5
",
            1,
            2
        ),
        None
    )
}

#[test]
fn test_parse_conflict_simple() {
    insta::assert_debug_snapshot!(
        parse_conflict(
            b"line 1
<<<<<<<
%%%%%%%
 line 2
-line 3
+left
 line 4
+++++++
right
>>>>>>>
line 5
",
            1,
            2
        ),
        @r###"
    Some(
        [
            Resolved(
                "line 1\n",
            ),
            Conflict {
                removes: [
                    "line 2\nline 3\nline 4\n",
                ],
                adds: [
                    "line 2\nleft\nline 4\n",
                    "right\n",
                ],
            },
            Resolved(
                "line 5\n",
            ),
        ],
    )
    "###
    )
}

#[test]
fn test_parse_conflict_multi_way() {
    insta::assert_debug_snapshot!(
        parse_conflict(
            b"line 1
<<<<<<<
%%%%%%%
 line 2
-line 3
+left
 line 4
+++++++
right
%%%%%%%
 line 2
+forward
 line 3
 line 4
>>>>>>>
line 5
",
            2,
            3
        ),
        @r###"
    Some(
        [
            Resolved(
                "line 1\n",
            ),
            Conflict {
                removes: [
                    "line 2\nline 3\nline 4\n",
                    "line 2\nline 3\nline 4\n",
                ],
                adds: [
                    "line 2\nleft\nline 4\n",
                    "right\n",
                    "line 2\nforward\nline 3\nline 4\n",
                ],
            },
            Resolved(
                "line 5\n",
            ),
        ],
    )
    "###
    )
}

#[test]
fn test_parse_conflict_different_wrong_arity() {
    assert_eq!(
        parse_conflict(
            b"line 1
<<<<<<<
%%%%%%%
 line 2
-line 3
+left
 line 4
+++++++
right
>>>>>>>
line 5
",
            2,
            3
        ),
        None
    )
}

#[test]
fn test_parse_conflict_malformed_marker() {
    // The conflict marker is missing `%%%%%%%`
    assert_eq!(
        parse_conflict(
            b"line 1
<<<<<<<
 line 2
-line 3
+left
 line 4
+++++++
right
>>>>>>>
line 5
",
            1,
            2
        ),
        None
    )
}

#[test]
fn test_parse_conflict_malformed_diff() {
    // The diff part is invalid (missing space before "line 4")
    assert_eq!(
        parse_conflict(
            b"line 1
<<<<<<<
%%%%%%%
 line 2
-line 3
+left
line 4
+++++++
right
>>>>>>>
line 5
",
            1,
            2
        ),
        None
    )
}

#[test]
fn test_update_conflict_from_content() {
    let test_repo = TestRepo::init(false);
    let store = test_repo.repo.store();

    let path = RepoPath::from_internal_string("dir/file");
    let base_file_id = testutils::write_file(store, &path, "line 1\nline 2\nline 3\n");
    let left_file_id = testutils::write_file(store, &path, "left 1\nline 2\nleft 3\n");
    let right_file_id = testutils::write_file(store, &path, "right 1\nline 2\nright 3\n");
    let conflict = Conflict {
        removes: vec![file_conflict_term(&base_file_id)],
        adds: vec![
            file_conflict_term(&left_file_id),
            file_conflict_term(&right_file_id),
        ],
    };
    let conflict_id = store.write_conflict(&path, &conflict).unwrap();

    // If the content is unchanged compared to the materialized value, we get the
    // old conflict id back.
    let mut materialized = vec![];
    materialize_conflict(store, &path, &conflict, &mut materialized).unwrap();
    let result = update_conflict_from_content(store, &path, &conflict_id, &materialized).unwrap();
    assert_eq!(result, Some(conflict_id.clone()));

    // If the conflict is resolved, we None back to indicate that.
    let result = update_conflict_from_content(
        store,
        &path,
        &conflict_id,
        b"resolved 1\nline 2\nresolved 3\n",
    )
    .unwrap();
    assert_eq!(result, None);

    // If the conflict is partially resolved, we get a new conflict back.
    let result = update_conflict_from_content(
        store,
        &path,
        &conflict_id,
        b"resolved 1\nline 2\n<<<<<<<\n%%%%%%%\n-line 3\n+left 3\n+++++++\nright 3\n>>>>>>>\n",
    )
    .unwrap();
    assert_ne!(result, None);
    assert_ne!(result, Some(conflict_id));
    let new_conflict = store.read_conflict(&path, &result.unwrap()).unwrap();
    // Calculate expected new FileIds
    let new_base_file_id = testutils::write_file(store, &path, "resolved 1\nline 2\nline 3\n");
    let new_left_file_id = testutils::write_file(store, &path, "resolved 1\nline 2\nleft 3\n");
    let new_right_file_id = testutils::write_file(store, &path, "resolved 1\nline 2\nright 3\n");
    assert_eq!(
        new_conflict,
        Conflict {
            removes: vec![file_conflict_term(&new_base_file_id)],
            adds: vec![
                file_conflict_term(&new_left_file_id),
                file_conflict_term(&new_right_file_id)
            ]
        }
    )
}

fn materialize_conflict_string(store: &Store, path: &RepoPath, conflict: &Conflict) -> String {
    let mut result: Vec<u8> = vec![];
    materialize_conflict(store, path, conflict, &mut result).unwrap();
    String::from_utf8(result).unwrap()
}
