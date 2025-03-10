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

use std::collections::{HashMap, HashSet};
use std::fmt::{Debug, Formatter};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use itertools::Itertools;
use once_cell::sync::OnceCell;
use thiserror::Error;

use self::dirty_cell::DirtyCell;
use crate::backend::{Backend, BackendError, BackendResult, ChangeId, CommitId, ObjectId, TreeId};
use crate::commit::Commit;
use crate::commit_builder::CommitBuilder;
use crate::dag_walk::topo_order_reverse;
use crate::default_index_store::{DefaultIndexStore, IndexEntry, IndexPosition};
use crate::git_backend::GitBackend;
use crate::index::{HexPrefix, Index, IndexStore, MutableIndex, PrefixResolution, ReadonlyIndex};
use crate::local_backend::LocalBackend;
use crate::op_heads_store::{self, OpHeadResolutionError, OpHeadsStore};
use crate::op_store::{BranchTarget, OpStore, OperationId, RefTarget, WorkspaceId};
use crate::operation::Operation;
use crate::refs::merge_ref_targets;
use crate::rewrite::DescendantRebaser;
use crate::settings::{RepoSettings, UserSettings};
use crate::simple_op_heads_store::SimpleOpHeadsStore;
use crate::simple_op_store::SimpleOpStore;
use crate::store::Store;
use crate::transaction::Transaction;
use crate::view::{RefName, View};
use crate::{backend, op_store};

pub trait Repo {
    fn base_repo(&self) -> &Arc<ReadonlyRepo>;

    fn store(&self) -> &Arc<Store>;

    fn op_store(&self) -> &Arc<dyn OpStore>;

    fn index(&self) -> &dyn Index;

    fn view(&self) -> &View;

    fn resolve_change_id(&self, change_id: &ChangeId) -> Option<Vec<IndexEntry>> {
        // Replace this if we added more efficient lookup method.
        let prefix = HexPrefix::from_bytes(change_id.as_bytes());
        match self.resolve_change_id_prefix(&prefix) {
            PrefixResolution::NoMatch => None,
            PrefixResolution::SingleMatch(entries) => Some(entries),
            PrefixResolution::AmbiguousMatch => panic!("complete change_id should be unambiguous"),
        }
    }

    fn resolve_change_id_prefix(&self, prefix: &HexPrefix) -> PrefixResolution<Vec<IndexEntry>>;

    fn shortest_unique_change_id_prefix_len(&self, target_id_bytes: &ChangeId) -> usize;
}

pub struct ReadonlyRepo {
    repo_path: PathBuf,
    store: Arc<Store>,
    op_store: Arc<dyn OpStore>,
    op_heads_store: Arc<dyn OpHeadsStore>,
    operation: Operation,
    settings: RepoSettings,
    index_store: Arc<dyn IndexStore>,
    index: OnceCell<Box<dyn ReadonlyIndex>>,
    // TODO: This should eventually become part of the index and not be stored fully in memory.
    change_id_index: OnceCell<ChangeIdIndex>,
    view: View,
}

impl Debug for ReadonlyRepo {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("Repo")
            .field("repo_path", &self.repo_path)
            .field("store", &self.store)
            .finish()
    }
}

impl ReadonlyRepo {
    pub fn default_op_store_factory() -> impl FnOnce(&Path) -> Box<dyn OpStore> {
        |store_path| Box::new(SimpleOpStore::init(store_path))
    }

    pub fn default_op_heads_store_factory() -> impl FnOnce(&Path) -> Box<dyn OpHeadsStore> {
        |store_path| {
            let store = SimpleOpHeadsStore::init(store_path);
            Box::new(store)
        }
    }

    pub fn default_index_store_factory() -> impl FnOnce(&Path) -> Box<dyn IndexStore> {
        |store_path| Box::new(DefaultIndexStore::init(store_path))
    }

    pub fn init(
        user_settings: &UserSettings,
        repo_path: &Path,
        backend_factory: impl FnOnce(&Path) -> Box<dyn Backend>,
        op_store_factory: impl FnOnce(&Path) -> Box<dyn OpStore>,
        op_heads_store_factory: impl FnOnce(&Path) -> Box<dyn OpHeadsStore>,
        index_store_factory: impl FnOnce(&Path) -> Box<dyn IndexStore>,
    ) -> Result<Arc<ReadonlyRepo>, PathError> {
        let repo_path = repo_path.canonicalize().context(repo_path)?;

        let store_path = repo_path.join("store");
        fs::create_dir(&store_path).context(&store_path)?;
        let backend = backend_factory(&store_path);
        let backend_path = store_path.join("type");
        fs::write(&backend_path, backend.name()).context(&backend_path)?;
        let store = Store::new(backend);
        let repo_settings = user_settings.with_repo(&repo_path).unwrap();

        let op_store_path = repo_path.join("op_store");
        fs::create_dir(&op_store_path).context(&op_store_path)?;
        let op_store = op_store_factory(&op_store_path);
        let op_store_type_path = op_store_path.join("type");
        fs::write(&op_store_type_path, op_store.name()).context(&op_store_type_path)?;
        let op_store: Arc<dyn OpStore> = Arc::from(op_store);

        let mut root_view = op_store::View::default();
        root_view.head_ids.insert(store.root_commit_id().clone());
        root_view
            .public_head_ids
            .insert(store.root_commit_id().clone());

        let op_heads_path = repo_path.join("op_heads");
        fs::create_dir(&op_heads_path).context(&op_heads_path)?;
        let operation_metadata =
            crate::transaction::create_op_metadata(user_settings, "initialize repo".to_string());
        let root_view_id = op_store.write_view(&root_view).unwrap();
        let init_operation = op_store::Operation {
            view_id: root_view_id,
            parents: vec![],
            metadata: operation_metadata,
        };
        let init_operation_id = op_store.write_operation(&init_operation).unwrap();
        let init_operation = Operation::new(op_store.clone(), init_operation_id, init_operation);
        let op_heads_store = op_heads_store_factory(&op_heads_path);
        op_heads_store.add_op_head(init_operation.id());
        let op_heads_type_path = op_heads_path.join("type");
        fs::write(&op_heads_type_path, op_heads_store.name()).context(&op_heads_type_path)?;
        let op_heads_store = Arc::from(op_heads_store);

        let index_path = repo_path.join("index");
        fs::create_dir(&index_path).context(&index_path)?;
        let index_store = index_store_factory(&index_path);
        let index_type_path = index_path.join("type");
        fs::write(&index_type_path, index_store.name()).context(&index_type_path)?;
        let index_store = Arc::from(index_store);

        let view = View::new(root_view);
        Ok(Arc::new(ReadonlyRepo {
            repo_path,
            store,
            op_store,
            op_heads_store,
            operation: init_operation,
            settings: repo_settings,
            index_store,
            index: OnceCell::new(),
            change_id_index: OnceCell::new(),
            view,
        }))
    }

    pub fn loader(&self) -> RepoLoader {
        RepoLoader {
            repo_path: self.repo_path.clone(),
            repo_settings: self.settings.clone(),
            store: self.store.clone(),
            op_store: self.op_store.clone(),
            op_heads_store: self.op_heads_store.clone(),
            index_store: self.index_store.clone(),
        }
    }

    pub fn repo_path(&self) -> &PathBuf {
        &self.repo_path
    }

    pub fn op_id(&self) -> &OperationId {
        self.operation.id()
    }

    pub fn operation(&self) -> &Operation {
        &self.operation
    }

    pub fn view(&self) -> &View {
        &self.view
    }

    pub fn readonly_index(&self) -> &dyn ReadonlyIndex {
        self.index
            .get_or_init(|| {
                self.index_store
                    .get_index_at_op(&self.operation, &self.store)
            })
            .as_ref()
    }

    fn change_id_index(&self) -> &ChangeIdIndex {
        self.change_id_index.get_or_init(|| {
            let heads = self.view().heads().iter().cloned().collect_vec();
            let walk = self.readonly_index().as_index().walk_revs(&heads, &[]);
            IdIndex::from_vec(
                walk.map(|entry| (entry.change_id(), entry.position()))
                    .collect(),
            )
        })
    }

    pub fn op_heads_store(&self) -> &Arc<dyn OpHeadsStore> {
        &self.op_heads_store
    }

    pub fn index_store(&self) -> &Arc<dyn IndexStore> {
        &self.index_store
    }

    pub fn settings(&self) -> &RepoSettings {
        &self.settings
    }

    pub fn start_transaction(
        self: &Arc<ReadonlyRepo>,
        user_settings: &UserSettings,
        description: &str,
    ) -> Transaction {
        let mut_repo = MutableRepo::new(self.clone(), self.readonly_index(), &self.view);
        Transaction::new(mut_repo, user_settings, description)
    }

    pub fn reload_at_head(
        &self,
        user_settings: &UserSettings,
    ) -> Result<Arc<ReadonlyRepo>, OpHeadResolutionError<BackendError>> {
        self.loader().load_at_head(user_settings)
    }

    pub fn reload_at(&self, operation: &Operation) -> Arc<ReadonlyRepo> {
        self.loader().load_at(operation)
    }
}

impl Repo for Arc<ReadonlyRepo> {
    fn base_repo(&self) -> &Arc<ReadonlyRepo> {
        self
    }

    fn store(&self) -> &Arc<Store> {
        &self.store
    }

    fn op_store(&self) -> &Arc<dyn OpStore> {
        &self.op_store
    }

    fn index(&self) -> &dyn Index {
        self.readonly_index().as_index()
    }

    fn view(&self) -> &View {
        &self.view
    }

    fn resolve_change_id_prefix(&self, prefix: &HexPrefix) -> PrefixResolution<Vec<IndexEntry>> {
        let index = self.index();
        self.change_id_index()
            .resolve_prefix_with(prefix, |&pos| index.entry_by_pos(pos))
    }

    fn shortest_unique_change_id_prefix_len(&self, target_id: &ChangeId) -> usize {
        self.change_id_index().shortest_unique_prefix_len(target_id)
    }
}

type BackendFactory = Box<dyn Fn(&Path) -> Box<dyn Backend>>;
type OpStoreFactory = Box<dyn Fn(&Path) -> Box<dyn OpStore>>;
type OpHeadsStoreFactory = Box<dyn Fn(&Path) -> Box<dyn OpHeadsStore>>;
type IndexStoreFactory = Box<dyn Fn(&Path) -> Box<dyn IndexStore>>;

pub struct StoreFactories {
    backend_factories: HashMap<String, BackendFactory>,
    op_store_factories: HashMap<String, OpStoreFactory>,
    op_heads_store_factories: HashMap<String, OpHeadsStoreFactory>,
    index_store_factories: HashMap<String, IndexStoreFactory>,
}

impl Default for StoreFactories {
    fn default() -> Self {
        let mut factories = StoreFactories::empty();

        // Backends
        factories.add_backend(
            "local",
            Box::new(|store_path| Box::new(LocalBackend::load(store_path))),
        );
        factories.add_backend(
            "git",
            Box::new(|store_path| Box::new(GitBackend::load(store_path))),
        );

        // OpStores
        factories.add_op_store(
            "simple_op_store",
            Box::new(|store_path| Box::new(SimpleOpStore::load(store_path))),
        );

        // OpHeadsStores
        factories.add_op_heads_store(
            "simple_op_heads_store",
            Box::new(|store_path| Box::new(SimpleOpHeadsStore::load(store_path))),
        );

        // Index
        factories.add_index_store(
            "default",
            Box::new(|store_path| Box::new(DefaultIndexStore::load(store_path))),
        );

        factories
    }
}

#[derive(Debug, Error)]
pub enum StoreLoadError {
    #[error("Unsupported {store} backend type '{store_type}'")]
    UnsupportedType {
        store: &'static str,
        store_type: String,
    },
    #[error("Failed to read {store} backend type: {source}")]
    ReadError {
        store: &'static str,
        source: io::Error,
    },
}

impl StoreFactories {
    pub fn empty() -> Self {
        StoreFactories {
            backend_factories: HashMap::new(),
            op_store_factories: HashMap::new(),
            op_heads_store_factories: HashMap::new(),
            index_store_factories: HashMap::new(),
        }
    }

    pub fn add_backend(&mut self, name: &str, factory: BackendFactory) {
        self.backend_factories.insert(name.to_string(), factory);
    }

    pub fn load_backend(&self, store_path: &Path) -> Result<Box<dyn Backend>, StoreLoadError> {
        // For compatibility with existing repos. TODO: Delete in 0.8+.
        if store_path.join("backend").is_file() {
            fs::rename(store_path.join("backend"), store_path.join("type"))
                .expect("Failed to rename 'backend' file to 'type'");
        }
        let backend_type = match fs::read_to_string(store_path.join("type")) {
            Ok(content) => content,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // For compatibility with existing repos. TODO: Delete in 0.8+.
                let inferred_type = if store_path.join("git_target").is_file() {
                    String::from("git")
                } else {
                    String::from("local")
                };
                fs::write(store_path.join("type"), &inferred_type).unwrap();
                inferred_type
            }
            Err(err) => {
                return Err(StoreLoadError::ReadError {
                    store: "commit",
                    source: err,
                });
            }
        };
        let backend_factory = self.backend_factories.get(&backend_type).ok_or_else(|| {
            StoreLoadError::UnsupportedType {
                store: "commit",
                store_type: backend_type.to_string(),
            }
        })?;
        Ok(backend_factory(store_path))
    }

    pub fn add_op_store(&mut self, name: &str, factory: OpStoreFactory) {
        self.op_store_factories.insert(name.to_string(), factory);
    }

    pub fn load_op_store(&self, store_path: &Path) -> Result<Box<dyn OpStore>, StoreLoadError> {
        let op_store_type = match fs::read_to_string(store_path.join("type")) {
            Ok(content) => content,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // For compatibility with existing repos. TODO: Delete in 0.8+
                let default_type = String::from("simple_op_store");
                fs::write(store_path.join("type"), &default_type).unwrap();
                default_type
            }
            Err(err) => {
                return Err(StoreLoadError::ReadError {
                    store: "operation",
                    source: err,
                });
            }
        };
        let op_store_factory = self.op_store_factories.get(&op_store_type).ok_or_else(|| {
            StoreLoadError::UnsupportedType {
                store: "operation",
                store_type: op_store_type.to_string(),
            }
        })?;
        Ok(op_store_factory(store_path))
    }

    pub fn add_op_heads_store(&mut self, name: &str, factory: OpHeadsStoreFactory) {
        self.op_heads_store_factories
            .insert(name.to_string(), factory);
    }

    pub fn load_op_heads_store(
        &self,
        store_path: &Path,
    ) -> Result<Box<dyn OpHeadsStore>, StoreLoadError> {
        let op_heads_store_type = match fs::read_to_string(store_path.join("type")) {
            Ok(content) => content,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // For compatibility with existing repos. TODO: Delete in 0.8+
                let default_type = String::from("simple_op_heads_store");
                fs::write(store_path.join("type"), &default_type).unwrap();
                default_type
            }
            Err(err) => {
                return Err(StoreLoadError::ReadError {
                    store: "operation heads",
                    source: err,
                });
            }
        };
        let op_heads_store_factory = self
            .op_heads_store_factories
            .get(&op_heads_store_type)
            .ok_or_else(|| StoreLoadError::UnsupportedType {
                store: "operation heads",
                store_type: op_heads_store_type.to_string(),
            })?;
        Ok(op_heads_store_factory(store_path))
    }

    pub fn add_index_store(&mut self, name: &str, factory: IndexStoreFactory) {
        self.index_store_factories.insert(name.to_string(), factory);
    }

    pub fn load_index_store(
        &self,
        store_path: &Path,
    ) -> Result<Box<dyn IndexStore>, StoreLoadError> {
        let index_store_type = match fs::read_to_string(store_path.join("type")) {
            Ok(content) => content,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // For compatibility with existing repos. TODO: Delete in 0.9+
                let default_type = String::from("default");
                fs::write(store_path.join("type"), &default_type).unwrap();
                default_type
            }
            Err(err) => {
                return Err(StoreLoadError::ReadError {
                    store: "index",
                    source: err,
                });
            }
        };
        let index_store_factory = self
            .index_store_factories
            .get(&index_store_type)
            .ok_or_else(|| StoreLoadError::UnsupportedType {
                store: "index",
                store_type: index_store_type.to_string(),
            })?;
        Ok(index_store_factory(store_path))
    }
}

#[derive(Clone)]
pub struct RepoLoader {
    repo_path: PathBuf,
    repo_settings: RepoSettings,
    store: Arc<Store>,
    op_store: Arc<dyn OpStore>,
    op_heads_store: Arc<dyn OpHeadsStore>,
    index_store: Arc<dyn IndexStore>,
}

impl RepoLoader {
    pub fn init(
        user_settings: &UserSettings,
        repo_path: &Path,
        store_factories: &StoreFactories,
    ) -> Result<Self, StoreLoadError> {
        let store = Store::new(store_factories.load_backend(&repo_path.join("store"))?);
        let repo_settings = user_settings.with_repo(repo_path).unwrap();
        let op_store = Arc::from(store_factories.load_op_store(&repo_path.join("op_store"))?);
        let op_heads_store =
            Arc::from(store_factories.load_op_heads_store(&repo_path.join("op_heads"))?);
        let index_store = Arc::from(store_factories.load_index_store(&repo_path.join("index"))?);
        Ok(Self {
            repo_path: repo_path.to_path_buf(),
            repo_settings,
            store,
            op_store,
            op_heads_store,
            index_store,
        })
    }

    pub fn repo_path(&self) -> &PathBuf {
        &self.repo_path
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn index_store(&self) -> &Arc<dyn IndexStore> {
        &self.index_store
    }

    pub fn op_store(&self) -> &Arc<dyn OpStore> {
        &self.op_store
    }

    pub fn op_heads_store(&self) -> &Arc<dyn OpHeadsStore> {
        &self.op_heads_store
    }

    pub fn load_at_head(
        &self,
        user_settings: &UserSettings,
    ) -> Result<Arc<ReadonlyRepo>, OpHeadResolutionError<BackendError>> {
        let op = op_heads_store::resolve_op_heads(
            self.op_heads_store.as_ref(),
            &self.op_store,
            |op_heads| self._resolve_op_heads(op_heads, user_settings),
        )?;
        let view = View::new(op.view().take_store_view());
        Ok(self._finish_load(op, view))
    }

    pub fn load_at(&self, op: &Operation) -> Arc<ReadonlyRepo> {
        let view = View::new(op.view().take_store_view());
        self._finish_load(op.clone(), view)
    }

    pub fn create_from(
        &self,
        operation: Operation,
        view: View,
        index: Box<dyn ReadonlyIndex>,
    ) -> Arc<ReadonlyRepo> {
        let repo = ReadonlyRepo {
            repo_path: self.repo_path.clone(),
            store: self.store.clone(),
            op_store: self.op_store.clone(),
            op_heads_store: self.op_heads_store.clone(),
            operation,
            settings: self.repo_settings.clone(),
            index_store: self.index_store.clone(),
            index: OnceCell::with_value(index),
            change_id_index: OnceCell::new(),
            view,
        };
        Arc::new(repo)
    }

    fn _resolve_op_heads(
        &self,
        op_heads: Vec<Operation>,
        user_settings: &UserSettings,
    ) -> Result<Operation, BackendError> {
        let base_repo = self.load_at(&op_heads[0]);
        let mut tx = base_repo.start_transaction(user_settings, "resolve concurrent operations");
        for other_op_head in op_heads.into_iter().skip(1) {
            tx.merge_operation(other_op_head);
            tx.mut_repo().rebase_descendants(user_settings)?;
        }
        let merged_repo = tx.write().leave_unpublished();
        Ok(merged_repo.operation().clone())
    }

    fn _finish_load(&self, operation: Operation, view: View) -> Arc<ReadonlyRepo> {
        let repo = ReadonlyRepo {
            repo_path: self.repo_path.clone(),
            store: self.store.clone(),
            op_store: self.op_store.clone(),
            op_heads_store: self.op_heads_store.clone(),
            operation,
            settings: self.repo_settings.clone(),
            index_store: self.index_store.clone(),
            index: OnceCell::new(),
            change_id_index: OnceCell::new(),
            view,
        };
        Arc::new(repo)
    }
}

pub struct MutableRepo {
    base_repo: Arc<ReadonlyRepo>,
    index: Box<dyn MutableIndex>,
    view: DirtyCell<View>,
    rewritten_commits: HashMap<CommitId, HashSet<CommitId>>,
    abandoned_commits: HashSet<CommitId>,
}

impl MutableRepo {
    pub fn new(
        base_repo: Arc<ReadonlyRepo>,
        index: &dyn ReadonlyIndex,
        view: &View,
    ) -> MutableRepo {
        let mut_view = view.clone();
        let mut_index = index.start_modification();
        MutableRepo {
            base_repo,
            index: mut_index,
            view: DirtyCell::with_clean(mut_view),
            rewritten_commits: Default::default(),
            abandoned_commits: Default::default(),
        }
    }

    fn view_mut(&mut self) -> &mut View {
        self.view.get_mut()
    }

    pub fn has_changes(&self) -> bool {
        !(self.abandoned_commits.is_empty()
            && self.rewritten_commits.is_empty()
            && self.view() == &self.base_repo.view)
    }

    pub fn consume(self) -> (Box<dyn MutableIndex>, View) {
        self.view.ensure_clean(|v| self.enforce_view_invariants(v));
        (self.index, self.view.into_inner())
    }

    pub fn new_commit(
        &mut self,
        settings: &UserSettings,
        parents: Vec<CommitId>,
        tree_id: TreeId,
    ) -> CommitBuilder {
        CommitBuilder::for_new_commit(self, settings, parents, tree_id)
    }

    pub fn rewrite_commit(
        &mut self,
        settings: &UserSettings,
        predecessor: &Commit,
    ) -> CommitBuilder {
        CommitBuilder::for_rewrite_from(self, settings, predecessor)
    }

    pub fn write_commit(&mut self, commit: backend::Commit) -> BackendResult<Commit> {
        let commit = self.store().write_commit(commit)?;
        self.add_head(&commit);
        Ok(commit)
    }

    /// Record a commit as having been rewritten in this transaction. This
    /// record is used by `rebase_descendants()`.
    ///
    /// Rewritten commits don't have to be recorded here. This is just a
    /// convenient place to record it. It won't matter after the transaction
    /// has been committed.
    pub fn record_rewritten_commit(&mut self, old_id: CommitId, new_id: CommitId) {
        assert_ne!(old_id, *self.store().root_commit_id());
        self.rewritten_commits
            .entry(old_id)
            .or_default()
            .insert(new_id);
    }

    pub fn clear_rewritten_commits(&mut self) {
        self.rewritten_commits.clear();
    }

    /// Record a commit as having been abandoned in this transaction. This
    /// record is used by `rebase_descendants()`.
    ///
    /// Abandoned commits don't have to be recorded here. This is just a
    /// convenient place to record it. It won't matter after the transaction
    /// has been committed.
    pub fn record_abandoned_commit(&mut self, old_id: CommitId) {
        assert_ne!(old_id, *self.store().root_commit_id());
        self.abandoned_commits.insert(old_id);
    }

    pub fn clear_abandoned_commits(&mut self) {
        self.abandoned_commits.clear();
    }

    pub fn has_rewrites(&self) -> bool {
        !(self.rewritten_commits.is_empty() && self.abandoned_commits.is_empty())
    }

    /// Creates a `DescendantRebaser` to rebase descendants of the recorded
    /// rewritten and abandoned commits.
    pub fn create_descendant_rebaser<'settings, 'repo>(
        &'repo mut self,
        settings: &'settings UserSettings,
    ) -> DescendantRebaser<'settings, 'repo> {
        DescendantRebaser::new(
            settings,
            self,
            self.rewritten_commits.clone(),
            self.abandoned_commits.clone(),
        )
    }

    pub fn rebase_descendants(&mut self, settings: &UserSettings) -> Result<usize, BackendError> {
        if !self.has_rewrites() {
            // Optimization
            return Ok(0);
        }
        let mut rebaser = self.create_descendant_rebaser(settings);
        rebaser.rebase_all()?;
        Ok(rebaser.rebased().len())
    }

    pub fn set_wc_commit(
        &mut self,
        workspace_id: WorkspaceId,
        commit_id: CommitId,
    ) -> Result<(), RewriteRootCommit> {
        if &commit_id == self.store().root_commit_id() {
            return Err(RewriteRootCommit);
        }
        self.view_mut().set_wc_commit(workspace_id, commit_id);
        Ok(())
    }

    pub fn remove_wc_commit(&mut self, workspace_id: &WorkspaceId) {
        self.view_mut().remove_wc_commit(workspace_id);
    }

    pub fn check_out(
        &mut self,
        workspace_id: WorkspaceId,
        settings: &UserSettings,
        commit: &Commit,
    ) -> Result<Commit, CheckOutCommitError> {
        let wc_commit = self
            .new_commit(
                settings,
                vec![commit.id().clone()],
                commit.tree_id().clone(),
            )
            .write()?;
        self.edit(workspace_id, &wc_commit)?;
        Ok(wc_commit)
    }

    pub fn edit(
        &mut self,
        workspace_id: WorkspaceId,
        commit: &Commit,
    ) -> Result<(), EditCommitError> {
        let maybe_wc_commit_id = self
            .view
            .with_ref(|v| v.get_wc_commit_id(&workspace_id).cloned());
        if let Some(wc_commit_id) = maybe_wc_commit_id {
            let wc_commit = self
                .store()
                .get_commit(&wc_commit_id)
                .map_err(EditCommitError::WorkingCopyCommitNotFound)?;
            if wc_commit.is_discardable() && self.view().heads().contains(wc_commit.id()) {
                // Abandon the working-copy commit we're leaving if it's empty and a head commit
                self.record_abandoned_commit(wc_commit_id);
            }
        }
        self.set_wc_commit(workspace_id, commit.id().clone())
            .map_err(|RewriteRootCommit| EditCommitError::RewriteRootCommit)
    }

    fn enforce_view_invariants(&self, view: &mut View) {
        let view = view.store_view_mut();
        view.public_head_ids = self
            .index()
            .heads(&mut view.public_head_ids.iter())
            .iter()
            .cloned()
            .collect();
        view.head_ids.extend(view.public_head_ids.iter().cloned());
        view.head_ids = self
            .index()
            .heads(&mut view.head_ids.iter())
            .iter()
            .cloned()
            .collect();
    }

    pub fn add_head(&mut self, head: &Commit) {
        let current_heads = self.view.get_mut().heads();
        // Use incremental update for common case of adding a single commit on top a
        // current head. TODO: Also use incremental update when adding a single
        // commit on top a non-head.
        if head
            .parent_ids()
            .iter()
            .all(|parent_id| current_heads.contains(parent_id))
        {
            self.index.add_commit(head);
            self.view.get_mut().add_head(head.id());
            for parent_id in head.parent_ids() {
                self.view.get_mut().remove_head(parent_id);
            }
        } else {
            let missing_commits = topo_order_reverse(
                vec![head.clone()],
                Box::new(|commit: &Commit| commit.id().clone()),
                Box::new(|commit: &Commit| -> Vec<Commit> {
                    commit
                        .parents()
                        .into_iter()
                        .filter(|parent| !self.index().has_id(parent.id()))
                        .collect()
                }),
            );
            for missing_commit in missing_commits.iter().rev() {
                self.index.add_commit(missing_commit);
            }
            self.view.get_mut().add_head(head.id());
            self.view.mark_dirty();
        }
    }

    pub fn remove_head(&mut self, head: &CommitId) {
        self.view_mut().remove_head(head);
        self.view.mark_dirty();
    }

    pub fn add_public_head(&mut self, head: &Commit) {
        self.view_mut().add_public_head(head.id());
        self.view.mark_dirty();
    }

    pub fn remove_public_head(&mut self, head: &CommitId) {
        self.view_mut().remove_public_head(head);
        self.view.mark_dirty();
    }

    pub fn get_branch(&self, name: &str) -> Option<BranchTarget> {
        self.view.with_ref(|v| v.get_branch(name).cloned())
    }

    pub fn set_branch(&mut self, name: String, target: BranchTarget) {
        self.view_mut().set_branch(name, target);
    }

    pub fn remove_branch(&mut self, name: &str) {
        self.view_mut().remove_branch(name);
    }

    pub fn get_local_branch(&self, name: &str) -> Option<RefTarget> {
        self.view.with_ref(|v| v.get_local_branch(name))
    }

    pub fn set_local_branch(&mut self, name: String, target: RefTarget) {
        self.view_mut().set_local_branch(name, target);
    }

    pub fn remove_local_branch(&mut self, name: &str) {
        self.view_mut().remove_local_branch(name);
    }

    pub fn get_remote_branch(&self, name: &str, remote_name: &str) -> Option<RefTarget> {
        self.view
            .with_ref(|v| v.get_remote_branch(name, remote_name))
    }

    pub fn set_remote_branch(&mut self, name: String, remote_name: String, target: RefTarget) {
        self.view_mut().set_remote_branch(name, remote_name, target);
    }

    pub fn remove_remote_branch(&mut self, name: &str, remote_name: &str) {
        self.view_mut().remove_remote_branch(name, remote_name);
    }

    pub fn rename_remote(&mut self, old: &str, new: &str) {
        self.view_mut().rename_remote(old, new);
    }

    pub fn get_tag(&self, name: &str) -> Option<RefTarget> {
        self.view.with_ref(|v| v.get_tag(name))
    }

    pub fn set_tag(&mut self, name: String, target: RefTarget) {
        self.view_mut().set_tag(name, target);
    }

    pub fn remove_tag(&mut self, name: &str) {
        self.view_mut().remove_tag(name);
    }

    pub fn get_git_ref(&self, name: &str) -> Option<RefTarget> {
        self.view.with_ref(|v| v.get_git_ref(name))
    }

    pub fn set_git_ref(&mut self, name: String, target: RefTarget) {
        self.view_mut().set_git_ref(name, target);
    }

    pub fn remove_git_ref(&mut self, name: &str) {
        self.view_mut().remove_git_ref(name);
    }

    pub fn set_git_head(&mut self, target: RefTarget) {
        self.view_mut().set_git_head(target);
    }

    pub fn clear_git_head(&mut self) {
        self.view_mut().clear_git_head();
    }

    pub fn set_view(&mut self, data: op_store::View) {
        self.view_mut().set_view(data);
        self.view.mark_dirty();
    }

    pub fn merge(&mut self, base_repo: &ReadonlyRepo, other_repo: &ReadonlyRepo) {
        // First, merge the index, so we can take advantage of a valid index when
        // merging the view. Merging in base_repo's index isn't typically
        // necessary, but it can be if base_repo is ahead of either self or other_repo
        // (e.g. because we're undoing an operation that hasn't been published).
        self.index.merge_in(base_repo.readonly_index());
        self.index.merge_in(other_repo.readonly_index());

        self.view.ensure_clean(|v| self.enforce_view_invariants(v));
        self.merge_view(&base_repo.view, &other_repo.view);
        self.view.mark_dirty();
    }

    fn merge_view(&mut self, base: &View, other: &View) {
        // Merge working-copy commits. If there's a conflict, we keep the self side.
        for (workspace_id, base_wc_commit) in base.wc_commit_ids() {
            let self_wc_commit = self.view().get_wc_commit_id(workspace_id);
            let other_wc_commit = other.get_wc_commit_id(workspace_id);
            if other_wc_commit == Some(base_wc_commit) || other_wc_commit == self_wc_commit {
                // The other side didn't change or both sides changed in the
                // same way.
            } else if let Some(other_wc_commit) = other_wc_commit {
                if self_wc_commit == Some(base_wc_commit) {
                    self.view_mut()
                        .set_wc_commit(workspace_id.clone(), other_wc_commit.clone());
                }
            } else {
                // The other side removed the workspace. We want to remove it even if the self
                // side changed the working-copy commit.
                self.view_mut().remove_wc_commit(workspace_id);
            }
        }
        for (workspace_id, other_wc_commit) in other.wc_commit_ids() {
            if self.view().get_wc_commit_id(workspace_id).is_none()
                && base.get_wc_commit_id(workspace_id).is_none()
            {
                // The other side added the workspace.
                self.view_mut()
                    .set_wc_commit(workspace_id.clone(), other_wc_commit.clone());
            }
        }

        for removed_head in base.public_heads().difference(other.public_heads()) {
            self.view_mut().remove_public_head(removed_head);
        }
        for added_head in other.public_heads().difference(base.public_heads()) {
            self.view_mut().add_public_head(added_head);
        }

        let base_heads = base.heads().iter().cloned().collect_vec();
        let own_heads = self.view().heads().iter().cloned().collect_vec();
        let other_heads = other.heads().iter().cloned().collect_vec();
        self.record_rewrites(&base_heads, &own_heads);
        self.record_rewrites(&base_heads, &other_heads);
        // No need to remove heads removed by `other` because we already marked them
        // abandoned or rewritten.
        for added_head in other.heads().difference(base.heads()) {
            self.view_mut().add_head(added_head);
        }

        let mut maybe_changed_ref_names = HashSet::new();

        let base_branches: HashSet<_> = base.branches().keys().cloned().collect();
        let other_branches: HashSet<_> = other.branches().keys().cloned().collect();
        for branch_name in base_branches.union(&other_branches) {
            let base_branch = base.branches().get(branch_name);
            let other_branch = other.branches().get(branch_name);
            if other_branch == base_branch {
                // Unchanged on other side
                continue;
            }

            maybe_changed_ref_names.insert(RefName::LocalBranch(branch_name.clone()));
            if let Some(branch) = base_branch {
                for remote in branch.remote_targets.keys() {
                    maybe_changed_ref_names.insert(RefName::RemoteBranch {
                        branch: branch_name.clone(),
                        remote: remote.clone(),
                    });
                }
            }
            if let Some(branch) = other_branch {
                for remote in branch.remote_targets.keys() {
                    maybe_changed_ref_names.insert(RefName::RemoteBranch {
                        branch: branch_name.clone(),
                        remote: remote.clone(),
                    });
                }
            }
        }

        for tag_name in base.tags().keys() {
            maybe_changed_ref_names.insert(RefName::Tag(tag_name.clone()));
        }
        for tag_name in other.tags().keys() {
            maybe_changed_ref_names.insert(RefName::Tag(tag_name.clone()));
        }

        for git_ref_name in base.git_refs().keys() {
            maybe_changed_ref_names.insert(RefName::GitRef(git_ref_name.clone()));
        }
        for git_ref_name in other.git_refs().keys() {
            maybe_changed_ref_names.insert(RefName::GitRef(git_ref_name.clone()));
        }

        for ref_name in maybe_changed_ref_names {
            let base_target = base.get_ref(&ref_name);
            let other_target = other.get_ref(&ref_name);
            self.view.get_mut().merge_single_ref(
                self.index.as_index(),
                &ref_name,
                base_target.as_ref(),
                other_target.as_ref(),
            );
        }

        if let Some(new_git_head) = merge_ref_targets(
            self.index(),
            self.view().git_head(),
            base.git_head(),
            other.git_head(),
        ) {
            self.set_git_head(new_git_head);
        } else {
            self.clear_git_head();
        }
    }

    /// Finds and records commits that were rewritten or abandoned between
    /// `old_heads` and `new_heads`.
    fn record_rewrites(&mut self, old_heads: &[CommitId], new_heads: &[CommitId]) {
        let mut removed_changes: HashMap<ChangeId, Vec<CommitId>> = HashMap::new();
        for removed in self.index().walk_revs(old_heads, new_heads) {
            removed_changes
                .entry(removed.change_id())
                .or_default()
                .push(removed.commit_id());
        }
        if removed_changes.is_empty() {
            return;
        }

        let mut rewritten_changes = HashSet::new();
        let mut rewritten_commits: HashMap<CommitId, Vec<CommitId>> = HashMap::new();
        for added in self.index().walk_revs(new_heads, old_heads) {
            let change_id = added.change_id();
            if let Some(old_commits) = removed_changes.get(&change_id) {
                for old_commit in old_commits {
                    rewritten_commits
                        .entry(old_commit.clone())
                        .or_default()
                        .push(added.commit_id());
                }
            }
            rewritten_changes.insert(change_id);
        }
        for (old_commit, new_commits) in rewritten_commits {
            for new_commit in new_commits {
                self.record_rewritten_commit(old_commit.clone(), new_commit);
            }
        }

        for (change_id, removed_commit_ids) in &removed_changes {
            if !rewritten_changes.contains(change_id) {
                for removed_commit_id in removed_commit_ids {
                    self.record_abandoned_commit(removed_commit_id.clone());
                }
            }
        }
    }

    pub fn merge_single_ref(
        &mut self,
        ref_name: &RefName,
        base_target: Option<&RefTarget>,
        other_target: Option<&RefTarget>,
    ) {
        self.view.get_mut().merge_single_ref(
            self.index.as_index(),
            ref_name,
            base_target,
            other_target,
        );
    }
}

impl Repo for MutableRepo {
    fn base_repo(&self) -> &Arc<ReadonlyRepo> {
        &self.base_repo
    }

    fn store(&self) -> &Arc<Store> {
        self.base_repo.store()
    }

    fn op_store(&self) -> &Arc<dyn OpStore> {
        self.base_repo.op_store()
    }

    fn index(&self) -> &dyn Index {
        self.index.as_index()
    }

    fn view(&self) -> &View {
        self.view
            .get_or_ensure_clean(|v| self.enforce_view_invariants(v))
    }

    fn resolve_change_id_prefix(&self, prefix: &HexPrefix) -> PrefixResolution<Vec<IndexEntry>> {
        // TODO: Create a persistent lookup from change id to (visible?) commit ids.
        let mut found_change_id = None;
        let mut found_entries = vec![];
        let heads = self.view().heads().iter().cloned().collect_vec();
        for entry in self.index().walk_revs(&heads, &[]) {
            let change_id = entry.change_id();
            if prefix.matches(&change_id) {
                if let Some(previous_change_id) = found_change_id.replace(change_id.clone()) {
                    if previous_change_id != change_id {
                        return PrefixResolution::AmbiguousMatch;
                    }
                }
                found_entries.push(entry);
            }
        }
        if found_change_id.is_none() {
            return PrefixResolution::NoMatch;
        }
        PrefixResolution::SingleMatch(found_entries)
    }

    fn shortest_unique_change_id_prefix_len(&self, target_id: &ChangeId) -> usize {
        target_id.as_bytes().len() * 2 // TODO
    }
}

/// Error from attempts to check out the root commit for editing
#[derive(Debug, Error)]
#[error("Cannot rewrite the root commit")]
pub struct RewriteRootCommit;

/// Error from attempts to edit a commit
#[derive(Debug, Error)]
pub enum EditCommitError {
    #[error("Current working-copy commit not found: {0}")]
    WorkingCopyCommitNotFound(BackendError),
    #[error("Cannot rewrite the root commit")]
    RewriteRootCommit,
}

/// Error from attempts to check out a commit
#[derive(Debug, Error)]
pub enum CheckOutCommitError {
    #[error("Failed to create new working-copy commit: {0}")]
    CreateCommit(#[from] BackendError),
    #[error("Failed to edit commit: {0}")]
    EditCommit(#[from] EditCommitError),
}

#[derive(Debug, Error)]
#[error("Cannot access {path}")]
pub struct PathError {
    pub path: PathBuf,
    #[source]
    pub error: io::Error,
}

pub(crate) trait IoResultExt<T> {
    fn context(self, path: impl AsRef<Path>) -> Result<T, PathError>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn context(self, path: impl AsRef<Path>) -> Result<T, PathError> {
        self.map_err(|error| PathError {
            path: path.as_ref().to_path_buf(),
            error,
        })
    }
}

mod dirty_cell {
    use std::cell::{Cell, RefCell};

    /// Cell that lazily updates the value after `mark_dirty()`.
    #[derive(Clone, Debug)]
    pub struct DirtyCell<T> {
        value: RefCell<T>,
        dirty: Cell<bool>,
    }

    impl<T> DirtyCell<T> {
        pub fn with_clean(value: T) -> Self {
            DirtyCell {
                value: RefCell::new(value),
                dirty: Cell::new(false),
            }
        }

        pub fn get_or_ensure_clean(&self, f: impl FnOnce(&mut T)) -> &T {
            // SAFETY: get_mut/mark_dirty(&mut self) should invalidate any previously-clean
            // references leaked by this method. Clean value never changes until then.
            self.ensure_clean(f);
            unsafe { &*self.value.as_ptr() }
        }

        pub fn ensure_clean(&self, f: impl FnOnce(&mut T)) {
            if self.dirty.get() {
                // This borrow_mut() ensures that there is no dirty temporary reference.
                // Panics if ensure_clean() is invoked from with_ref() callback for example.
                f(&mut self.value.borrow_mut());
                self.dirty.set(false);
            }
        }

        pub fn into_inner(self) -> T {
            self.value.into_inner()
        }

        pub fn with_ref<R>(&self, f: impl FnOnce(&T) -> R) -> R {
            f(&self.value.borrow())
        }

        pub fn get_mut(&mut self) -> &mut T {
            self.value.get_mut()
        }

        pub fn mark_dirty(&mut self) {
            *self.dirty.get_mut() = true;
        }
    }
}

type ChangeIdIndex = IdIndex<ChangeId, IndexPosition>;

#[derive(Debug, Clone)]
pub struct IdIndex<K, V>(Vec<(K, V)>);

impl<K, V> IdIndex<K, V>
where
    K: ObjectId + Ord,
{
    /// Creates new index from the given entries. Multiple values can be
    /// associated with a single key.
    pub fn from_vec(mut vec: Vec<(K, V)>) -> Self {
        vec.sort_unstable_by(|(k0, _), (k1, _)| k0.cmp(k1));
        IdIndex(vec)
    }

    /// Looks up entries with the given prefix, and collects values if matched
    /// entries have unambiguous keys.
    pub fn resolve_prefix_with<U>(
        &self,
        prefix: &HexPrefix,
        mut value_mapper: impl FnMut(&V) -> U,
    ) -> PrefixResolution<Vec<U>> {
        let mut range = self.resolve_prefix_range(prefix).peekable();
        if let Some((first_key, _)) = range.peek().copied() {
            let maybe_entries: Option<Vec<_>> = range
                .map(|(k, v)| (k == first_key).then(|| value_mapper(v)))
                .collect();
            if let Some(entries) = maybe_entries {
                PrefixResolution::SingleMatch(entries)
            } else {
                PrefixResolution::AmbiguousMatch
            }
        } else {
            PrefixResolution::NoMatch
        }
    }

    /// Iterates over entries with the given prefix.
    pub fn resolve_prefix_range<'a: 'b, 'b>(
        &'a self,
        prefix: &'b HexPrefix,
    ) -> impl Iterator<Item = (&'a K, &'a V)> + 'b {
        let min_bytes = prefix.min_prefix_bytes();
        let pos = self.0.partition_point(|(k, _)| k.as_bytes() < min_bytes);
        self.0[pos..]
            .iter()
            .take_while(|(k, _)| prefix.matches(k))
            .map(|(k, v)| (k, v))
    }

    /// This function returns the shortest length of a prefix of `key` that
    /// disambiguates it from every other key in the index.
    ///
    /// The length to be returned is a number of hexadecimal digits.
    ///
    /// This has some properties that we do not currently make much use of:
    ///
    /// - The algorithm works even if `key` itself is not in the index.
    ///
    /// - In the special case when there are keys in the trie for which our
    ///   `key` is an exact prefix, returns `key.len() + 1`. Conceptually, in
    ///   order to disambiguate, you need every letter of the key *and* the
    ///   additional fact that it's the entire key). This case is extremely
    ///   unlikely for hashes with 12+ hexadecimal characters.
    pub fn shortest_unique_prefix_len(&self, key: &K) -> usize {
        let pos = self.0.partition_point(|(k, _)| k < key);
        let left = pos.checked_sub(1).map(|p| &self.0[p]);
        let right = self.0[pos..].iter().find(|(k, _)| k != key);
        itertools::chain(left, right)
            .map(|(neighbor, _value)| {
                backend::common_hex_len(key.as_bytes(), neighbor.as_bytes()) + 1
            })
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id_index_resolve_prefix() {
        fn sorted(resolution: PrefixResolution<Vec<i32>>) -> PrefixResolution<Vec<i32>> {
            match resolution {
                PrefixResolution::SingleMatch(mut xs) => {
                    xs.sort(); // order of values might not be preserved by IdIndex
                    PrefixResolution::SingleMatch(xs)
                }
                _ => resolution,
            }
        }
        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("0000"), 0),
            (ChangeId::from_hex("0099"), 1),
            (ChangeId::from_hex("0099"), 2),
            (ChangeId::from_hex("0aaa"), 3),
            (ChangeId::from_hex("0aab"), 4),
        ]);
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("00").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("000").unwrap(), |&v| v),
            PrefixResolution::SingleMatch(vec![0]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0001").unwrap(), |&v| v),
            PrefixResolution::NoMatch,
        );
        assert_eq!(
            sorted(id_index.resolve_prefix_with(&HexPrefix::new("009").unwrap(), |&v| v)),
            PrefixResolution::SingleMatch(vec![1, 2]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0aa").unwrap(), |&v| v),
            PrefixResolution::AmbiguousMatch,
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("0aab").unwrap(), |&v| v),
            PrefixResolution::SingleMatch(vec![4]),
        );
        assert_eq!(
            id_index.resolve_prefix_with(&HexPrefix::new("f").unwrap(), |&v| v),
            PrefixResolution::NoMatch,
        );
    }

    #[test]
    fn test_id_index_shortest_unique_prefix_len() {
        // No crash if empty
        let id_index = IdIndex::from_vec(vec![] as Vec<(ChangeId, ())>);
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("00")),
            0
        );

        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("ab"), ()),
            (ChangeId::from_hex("acd0"), ()),
            (ChangeId::from_hex("acd0"), ()), // duplicated key is allowed
        ]);
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("acd0")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ac")),
            3
        );

        let id_index = IdIndex::from_vec(vec![
            (ChangeId::from_hex("ab"), ()),
            (ChangeId::from_hex("acd0"), ()),
            (ChangeId::from_hex("acf0"), ()),
            (ChangeId::from_hex("a0"), ()),
            (ChangeId::from_hex("ba"), ()),
        ]);

        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("a0")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ba")),
            1
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("ab")),
            2
        );
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("acd0")),
            3
        );
        // If it were there, the length would be 1.
        assert_eq!(
            id_index.shortest_unique_prefix_len(&ChangeId::from_hex("c0")),
            1
        );
    }
}
