use std::{
    fs, io, mem,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures_util::{StreamExt, stream::FuturesUnordered};
use log::{debug, error, info};
use mlua::{AnyUserData, FromLua, IntoLua, Lua};
use petgraph::{
    Direction,
    graph::{DiGraph, NodeIndex},
    visit::{Dfs, EdgeRef},
};
use thiserror::Error;
use tokio::sync::{AcquireError, Semaphore};

use crate::{
    executor::{self, Executor, LuaExecutor, MODULE_NAME},
    package::Package,
    planner::{LinkTime, Planner},
    store,
    utils::passthru::PassthruHashSet,
};

#[derive(Error, Debug)]
pub enum Error {
    #[error("could not acquire build permit")]
    AcquireError(#[from] AcquireError),
    #[error(transparent)]
    IOError(#[from] io::Error),
    #[error(transparent)]
    StoreError(#[from] store::Error),
    #[error(transparent)]
    ExecutorError(#[from] executor::Error),
    #[error(transparent)]
    LuaError(#[from] mlua::Error),
}

#[derive(Debug, Clone)]
pub struct BuilderOptions {
    pub concurrent: usize,
    pub root: PathBuf,
}

#[derive(Debug)]
enum PackageState {
    Unbuilt {
        package: Package,
        remaining: usize,
    },
    Building,
    Built {
        package: Package,
        runtime: Vec<NodeIndex>,
    },
}

struct BuildModules {
    executors: Vec<(String, Box<dyn Fn(Arc<Path>, &Lua) -> Result<AnyUserData, mlua::Error>>)>,
    lua: Lua,
    root: PathBuf,
    semaphore: Semaphore,
}

struct BuildInfo {
    node: NodeIndex,
    package: Package,
    runtime: Vec<NodeIndex>,
    buildtime: Vec<NodeIndex>,
}

/// Package build runner
///
/// The builder traverses through a [`Planner`]'s instructions and builds all of the environments needed to link the target package
pub struct Builder {
    state: DiGraph<PackageState, LinkTime>,
    modules: Arc<BuildModules>,
}

#[cold]
fn invalid_state(node: NodeIndex, state: &PackageState) -> ! {
    panic!("node {node:?} should not be in the {state:?} state")
}

impl Builder {
    pub fn new(planner: Planner, lua: Lua, options: BuilderOptions) -> Self {
        let mut state = planner.into_inner().into_inner().map_owned(
            |_, weight| PackageState::Unbuilt {
                remaining: 0,
                package: weight,
            },
            |_, weight| weight,
        );

        for node in state.node_indices() {
            let count = state.neighbors_directed(node, Direction::Outgoing).count();
            match state[node] {
                PackageState::Unbuilt {
                    ref mut remaining, ..
                } => *remaining = count,
                _ => unreachable!(),
            }
        }

        Self {
            state,
            modules: Arc::new(BuildModules {
                executors: Vec::new(),
                lua,
                root: options.root,
                semaphore: Semaphore::new(options.concurrent),
            }),
        }
    }

    pub fn with_executor<F, E>(&mut self, name: String, func: F) -> &mut Self
    where
        F: Fn(Arc<Path>) -> E + 'static,
        E: Executor + Send + 'static,
        E::Request: FromLua + IntoLua,
        E::Response: IntoLua,
    {
        let modules =
            Arc::get_mut(&mut self.modules).expect("only 1 reference to modules should exist");
        modules.executors.push((
            name,
            Box::new(move |path, lua| lua.create_userdata(LuaExecutor(func(path)))),
        ));
        self
    }

    fn environment_dir(root: &Path, node: NodeIndex) -> PathBuf {
        root.join(node.index().to_string())
    }

    pub async fn build(&mut self, target: NodeIndex) {
        let mut futures = FuturesUnordered::new();
        let mut subset = PassthruHashSet::default();

        // construct subgraph and build leaf packages
        let mut visitor = Dfs::new(&self.state, target);
        while let Some(node) = visitor.next(&self.state) {
            subset.insert(node);

            if let Some(info) = self.prepare_build(node) {
                debug!("adding package {} as a leaf", info.package.id);
                futures.push(build_impl(self.modules.clone(), info));
            }
        }

        // main build loop
        // TODO: write out builds result somewhere
        while let Some(finished) = futures.next().await {
            let finished = match finished {
                Ok(info) => info,
                Err((info, err)) => {
                    error!("could not build package {}: {err}", info.package.id);
                    self.state[info.node] = PackageState::Unbuilt {
                        package: info.package,
                        remaining: 0,
                    };
                    continue;
                }
            };

            self.state[finished.node] = PackageState::Built {
                runtime: finished.runtime,
                package: finished.package,
            };

            for parent in self
                .state
                .neighbors_directed(finished.node, Direction::Incoming)
                .filter(|node| subset.contains(node))
                .collect::<Vec<_>>()
            {
                match &mut self.state[parent] {
                    PackageState::Unbuilt { remaining, package } => {
                        *remaining -= 1;
                        debug!("{} has {} dependencies remaining", package.id, remaining);
                    }
                    state => invalid_state(parent, state),
                }

                if let Some(info) = self.prepare_build(parent) {
                    futures.push(build_impl(self.modules.clone(), info));
                }
            }
        }
    }

    fn prepare_build(&mut self, node: NodeIndex) -> Option<BuildInfo> {
        let pkg_state = &mut self.state[node];

        // check if package can be built
        match pkg_state {
            PackageState::Unbuilt { remaining, .. } if *remaining == 0 => (),
            _ => return None,
        };

        // set state to building and get pkg
        let package = match mem::replace(pkg_state, PackageState::Building) {
            PackageState::Unbuilt { package, .. } => package,
            _ => unreachable!(),
        };

        // gather dependencies into the build closure
        let mut buildtime = Vec::default();
        let mut runtime = Vec::default();
        for edge in self.state.edges_directed(node, Direction::Outgoing) {
            let child = edge.target();
            match &self.state[child] {
                PackageState::Built {
                    runtime: dep_runtime,
                    ..
                } => {
                    let closure = match edge.weight() {
                        LinkTime::Runtime => &mut runtime,
                        LinkTime::Buildtime => &mut buildtime,
                    };

                    closure.extend(dep_runtime.into_iter());
                    closure.push(child);
                }
                state => invalid_state(child, state),
            }
        }

        Some(BuildInfo {
            node,
            package,
            runtime,
            buildtime,
        })
    }
}

async fn build_impl(
    modules: Arc<BuildModules>,
    info: BuildInfo,
) -> Result<BuildInfo, (BuildInfo, Error)> {
    match build_impl_impl(modules, &info).await {
        Ok(()) => Ok(info),
        Err(err) => Err((info, err)),
    }
}

async fn build_impl_impl(modules: Arc<BuildModules>, info: &BuildInfo) -> Result<(), Error> {
    debug!("awaiting permit to build package {}", info.package.id);
    let permit = modules.semaphore.acquire().await?;
    info!("building package {}", info.package.id);

    let lua = &modules.lua;

    // create environment
    // TODO: link dependencies
    let environment = Arc::from(Builder::environment_dir(&modules.root, info.node));
    fs::create_dir(&environment)?;

    // register executors
    let executors = modules
        .executors
        .iter()
        .map(|(name, func)| Ok((name.clone(), func(environment.clone(), lua)?)))
        .collect::<Result<Vec<_>, mlua::Error>>()?;
    let executors = lua.create_table_from(executors)?;
    lua.register_module(MODULE_NAME, &executors)?;

    // build pkg
    info.package.build().await?;

    // cleanup
    executors.for_each::<String, AnyUserData>(|_, executor| executor.destroy())?;
    drop(permit);
    Ok(())
}
