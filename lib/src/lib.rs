// Copyright 2020 The Jujutsu Authors
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

#![deny(unused_must_use)]

#[macro_use]
mod content_hash;

pub mod backend;
pub mod commit;
pub mod commit_builder;
pub mod conflicts;
pub mod dag_walk;
pub mod default_index_store;
pub mod default_revset_engine;
pub mod default_revset_graph_iterator;
pub mod diff;
pub mod file_util;
pub mod files;
pub mod git;
pub mod git_backend;
pub mod gitignore;
pub mod hex_util;
pub mod index;
#[cfg(feature = "legacy-thrift")]
mod legacy_thrift_op_store;
pub mod local_backend;
pub mod lock;
pub mod matchers;
pub mod nightly_shims;
pub mod op_heads_store;
pub mod op_store;
pub mod operation;
mod proto_op_store;
pub mod protos;
pub mod refs;
pub mod repo;
pub mod repo_path;
pub mod revset;
pub mod rewrite;
pub mod settings;
pub mod simple_op_heads_store;
pub mod simple_op_store;
#[cfg(feature = "legacy-thrift")]
mod simple_op_store_model;
pub mod stacked_table;
pub mod store;
pub mod transaction;
pub mod tree;
pub mod tree_builder;
pub mod view;
pub mod working_copy;
pub mod workspace;
