use std::{
  cmp,
  cmp::Ordering,
  sync::{
    atomic::{AtomicU32, Ordering::Relaxed},
    Arc, LazyLock, RwLock,
  },
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use rspack_collections::IdentifierMap;
use rspack_core::{
  ApplyContext, BoxModule, Compilation, CompilationAfterOptimizeModules,
  CompilationAfterProcessAssets, CompilationBuildModule, CompilationChunkIds,
  CompilationFinishModules, CompilationId, CompilationModuleIds, CompilationOptimizeChunkModules,
  CompilationOptimizeChunks, CompilationOptimizeDependencies, CompilationOptimizeModules,
  CompilationOptimizeTree, CompilationParams, CompilationProcessAssets, CompilationSeal,
  CompilationSucceedModule, CompilerAfterEmit, CompilerCompilation, CompilerEmit,
  CompilerFinishMake, CompilerId, CompilerMake, CompilerOptions, CompilerThisCompilation,
  ModuleIdentifier, Plugin, PluginContext,
};
use rspack_error::Result;
use rspack_hook::{plugin, plugin_hook};

type HandlerFn = Arc<dyn Fn(f64, String, Vec<String>) -> Result<()> + Send + Sync>;

pub enum ProgressPluginOptions {
  Handler(HandlerFn),
  Default(ProgressPluginDisplayOptions),
}

impl std::fmt::Debug for ProgressPluginOptions {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ProgressPluginOptions::Handler(_handler) => {
        f.debug_struct("ProgressPluginOptions::Handler").finish()
      }
      ProgressPluginOptions::Default(options) => f
        .debug_struct("ProgressPluginOptions::Default")
        .field("options", &options)
        .finish(),
    }
  }
}

static MULTI_PROGRESS: LazyLock<MultiProgress> =
  LazyLock::new(|| MultiProgress::with_draw_target(ProgressDrawTarget::stdout_with_hz(100)));
#[derive(Debug, Default)]
pub struct ProgressPluginDisplayOptions {
  // the prefix name of progress bar
  pub prefix: String,
  // tells ProgressPlugin to collect profile data for progress steps.
  pub profile: bool,
  // the template of progress bar, see [`indicatif::ProgressStyle::with_template`]
  pub template: String,
  // the tick string sequence for spinners, see [`indicatif::ProgressStyle::tick_strings`]
  pub tick_strings: Option<Vec<String>>,
  // the progress characters, see [`indicatif::ProgressStyle::progress_chars`]
  pub progress_chars: String,
}

#[derive(Debug)]
pub struct ProgressPluginStateInfo {
  pub value: String,
  pub time: Instant,
  pub duration: Option<Duration>,
}

#[plugin]
#[derive(Debug)]
pub struct ProgressPlugin {
  pub options: ProgressPluginOptions,
  pub progress_bar: Option<ProgressBar>,
  pub modules_count: AtomicU32,
  pub modules_done: AtomicU32,
  pub active_modules: RwLock<IdentifierMap<Instant>>,
  pub last_modules_count: RwLock<Option<u32>>,
  pub last_active_module: RwLock<Option<ModuleIdentifier>>,
  pub last_state_info: RwLock<Vec<ProgressPluginStateInfo>>,
  pub last_updated: AtomicU32,
}

impl ProgressPlugin {
  pub fn new(options: ProgressPluginOptions) -> Self {
    let progress_bar = match &options {
      ProgressPluginOptions::Handler(_fn) => None,
      ProgressPluginOptions::Default(options) => {
        // default interval is 20, means draw every 1000/20 = 50ms, use 100 to draw every 1000/100 = 10ms
        let progress_bar = MULTI_PROGRESS.add(ProgressBar::new(100));

        let mut progress_bar_style = ProgressStyle::with_template(&options.template)
          .expect("TODO:")
          .progress_chars(&options.progress_chars);
        if let Some(tick_strings) = &options.tick_strings {
          progress_bar_style = progress_bar_style.tick_strings(
            tick_strings
              .iter()
              .map(|s| s.as_str())
              .collect::<Vec<_>>()
              .as_slice(),
          );
        }
        progress_bar.set_style(progress_bar_style);
        Some(progress_bar)
      }
    };
    Self::new_inner(
      options,
      progress_bar,
      AtomicU32::new(0),
      AtomicU32::new(0),
      Default::default(),
      Default::default(),
      Default::default(),
      Default::default(),
      AtomicU32::new(0),
    )
  }

  fn update_throttled(&self) -> Result<()> {
    let current_time = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("failed to get current time")
      .as_millis() as u32;

    if current_time - self.last_updated.load(Relaxed) > 200 {
      self.update()?;
      self.last_updated.store(current_time, Relaxed);
    }

    Ok(())
  }

  fn update(&self) -> Result<()> {
    let modules_done = self.modules_done.load(Relaxed);
    let percent_by_module = (modules_done as f64)
      / (cmp::max(
        self.last_modules_count.read().expect("TODO:").unwrap_or(1),
        self.modules_count.load(Relaxed),
      ) as f64);

    let mut items = vec![];
    let last_active_module = self.last_active_module.read().expect("TODO:");

    if let Some(last_active_module) = *last_active_module {
      items.push(last_active_module.to_string());
      let duration = self
        .active_modules
        .read()
        .expect("TODO:")
        .get(&last_active_module)
        .map(|time| Instant::now() - *time);
      self.handler(
        0.1 + percent_by_module * 0.55,
        String::from("building"),
        items,
        duration,
      )?;
    }
    Ok(())
  }

  pub fn handler(
    &self,
    percent: f64,
    msg: String,
    state_items: Vec<String>,
    time: Option<Duration>,
  ) -> Result<()> {
    match &self.options {
      ProgressPluginOptions::Handler(handler) => handler(percent, msg, state_items)?,
      ProgressPluginOptions::Default(options) => {
        if options.profile {
          self.default_handler(percent, msg, state_items, time);
        } else {
          self.progress_bar_handler(percent, msg, state_items);
        }
      }
    };
    Ok(())
  }

  fn default_handler(&self, _: f64, msg: String, items: Vec<String>, duration: Option<Duration>) {
    let full_state = [vec![msg.clone()], items.clone()].concat();
    let now = Instant::now();
    {
      let mut last_state_info = self.last_state_info.write().expect("TODO:");
      let len = full_state.len().max(last_state_info.len());
      let original_last_state_info_len = last_state_info.len();
      for i in (0..len).rev() {
        if i + 1 > original_last_state_info_len {
          last_state_info.insert(
            original_last_state_info_len,
            ProgressPluginStateInfo {
              value: full_state[i].clone(),
              time: now,
              duration: None,
            },
          )
        } else if i + 1 > full_state.len() || !last_state_info[i].value.eq(&full_state[i]) {
          let diff = match last_state_info[i].duration {
            Some(duration) => duration,
            _ => now - last_state_info[i].time,
          }
          .as_millis();
          let report_state = if i > 0 {
            last_state_info[i - 1].value.clone() + " > " + last_state_info[i].value.clone().as_str()
          } else {
            last_state_info[i].value.clone()
          };

          if diff > 5 {
            // TODO: color map
            let mut color = "\x1b[32m";
            if diff > 10000 {
              color = "\x1b[31m"
            } else if diff > 1000 {
              color = "\x1b[33m"
            }
            println!(
              "{}{} {} ms {}\x1B[0m",
              color,
              " | ".repeat(i),
              diff,
              report_state
            );
          }
          match (i + 1).cmp(&full_state.len()) {
            Ordering::Greater => last_state_info.truncate(i),
            Ordering::Equal => {
              last_state_info[i] = ProgressPluginStateInfo {
                value: full_state[i].clone(),
                time: now,
                duration,
              }
            }
            Ordering::Less => {
              last_state_info[i] = ProgressPluginStateInfo {
                value: full_state[i].clone(),
                time: now,
                duration: None,
              };
            }
          }
        }
      }
    }
  }

  fn progress_bar_handler(&self, percent: f64, msg: String, state_items: Vec<String>) {
    if let Some(progress_bar) = &self.progress_bar {
      let msg = msg + " " + state_items.join(" ").as_str();
      if percent == 1.0 {
        progress_bar.finish_with_message(msg);
      } else {
        progress_bar.set_message(msg);
        progress_bar.set_position((percent * 100.0) as u64);
      }
    }
  }

  fn sealing_hooks_report(&self, name: &str, index: i32) -> Result<()> {
    let number_of_sealing_hooks = 38;
    self.handler(
      0.7 + 0.25 * (index as f64 / number_of_sealing_hooks as f64),
      "sealing".to_string(),
      vec![name.to_string()],
      None,
    )
  }

  pub fn is_profile(&self) -> bool {
    match &self.options {
      ProgressPluginOptions::Handler(_) => false,
      ProgressPluginOptions::Default(options) => options.profile,
    }
  }
}

#[plugin_hook(CompilerThisCompilation for ProgressPlugin)]
async fn this_compilation(
  &self,
  _compilation: &mut Compilation,
  _params: &mut CompilationParams,
) -> Result<()> {
  if let ProgressPluginOptions::Default(options) = &self.options {
    let progress_bar = self.progress_bar.as_ref().unwrap_or_else(|| unreachable!());
    if !options.profile {
      progress_bar.reset();
      progress_bar.set_prefix(options.prefix.clone());
    }
  }

  self.handler(
    0.08,
    "setup".to_string(),
    vec!["compilation".to_string()],
    None,
  )
}

#[plugin_hook(CompilerCompilation for ProgressPlugin)]
async fn compilation(
  &self,
  _compilation: &mut Compilation,
  _params: &mut CompilationParams,
) -> Result<()> {
  self.handler(
    0.09,
    "setup".to_string(),
    vec!["compilation".to_string()],
    None,
  )
}

#[plugin_hook(CompilerMake for ProgressPlugin)]
async fn make(&self, _compilation: &mut Compilation) -> Result<()> {
  self.handler(0.1, String::from("make"), vec![], None)?;
  self.modules_count.store(0, Relaxed);
  self.modules_done.store(0, Relaxed);
  Ok(())
}

#[plugin_hook(CompilationBuildModule for ProgressPlugin)]
async fn build_module(
  &self,
  _compiler_id: CompilerId,
  _compilation_id: CompilationId,
  module: &mut BoxModule,
) -> Result<()> {
  self
    .active_modules
    .write()
    .expect("TODO:")
    .insert(module.identifier(), Instant::now());
  self.modules_count.fetch_add(1, Relaxed);
  self
    .last_active_module
    .write()
    .expect("TODO:")
    .replace(module.identifier());
  if let ProgressPluginOptions::Default(options) = &self.options {
    if !options.profile {
      self.update_throttled()?;
    }
  }

  Ok(())
}

#[plugin_hook(CompilationSucceedModule for ProgressPlugin)]
async fn succeed_module(
  &self,
  _compiler_id: CompilerId,
  _compilation_id: CompilationId,
  module: &mut BoxModule,
) -> Result<()> {
  self.modules_done.fetch_add(1, Relaxed);
  self
    .last_active_module
    .write()
    .expect("TODO:")
    .replace(module.identifier());

  // only profile mode should update at succeed module
  if self.is_profile() {
    self.update_throttled()?;
  }
  let mut last_active_module = Default::default();
  {
    let mut active_modules = self.active_modules.write().expect("TODO:");
    active_modules.remove(&module.identifier());

    // get the last active module
    if !self.is_profile() {
      active_modules.iter().for_each(|(module, _)| {
        last_active_module = *module;
      });
    }
  }
  if !self.is_profile() {
    self
      .last_active_module
      .write()
      .expect("TODO:")
      .replace(last_active_module);
    if !last_active_module.is_empty() {
      self.update_throttled()?;
    }
  }
  Ok(())
}

#[plugin_hook(CompilerFinishMake for ProgressPlugin)]
async fn finish_make(&self, _compilation: &mut Compilation) -> Result<()> {
  self.handler(
    0.69,
    "building".to_string(),
    vec!["finish make".to_string()],
    None,
  )
}

#[plugin_hook(CompilationFinishModules for ProgressPlugin)]
async fn finish_modules(&self, _compilation: &mut Compilation) -> Result<()> {
  self.sealing_hooks_report("finish modules", 0)
}

#[plugin_hook(CompilationSeal for ProgressPlugin)]
async fn seal(&self, _compilation: &mut Compilation) -> Result<()> {
  self.sealing_hooks_report("plugins", 1)
}

#[plugin_hook(CompilationOptimizeDependencies for ProgressPlugin)]
fn optimize_dependencies(&self, _compilation: &mut Compilation) -> Result<Option<bool>> {
  self.sealing_hooks_report("dependencies", 2)?;
  Ok(None)
}

#[plugin_hook(CompilationOptimizeModules for ProgressPlugin)]
async fn optimize_modules(&self, _compilation: &mut Compilation) -> Result<Option<bool>> {
  self.sealing_hooks_report("module optimization", 7)?;
  Ok(None)
}

#[plugin_hook(CompilationAfterOptimizeModules for ProgressPlugin)]
async fn after_optimize_modules(&self, _compilation: &mut Compilation) -> Result<()> {
  self.sealing_hooks_report("after module optimization", 8)
}

#[plugin_hook(CompilationOptimizeChunks for ProgressPlugin)]
async fn optimize_chunks(&self, _compilation: &mut Compilation) -> Result<Option<bool>> {
  self.sealing_hooks_report("chunk optimization", 9)?;
  Ok(None)
}

#[plugin_hook(CompilationOptimizeTree for ProgressPlugin)]
async fn optimize_tree(&self, _compilation: &mut Compilation) -> Result<()> {
  self.sealing_hooks_report("module and chunk tree optimization", 11)
}

#[plugin_hook(CompilationOptimizeChunkModules for ProgressPlugin)]
async fn optimize_chunk_modules(&self, _compilation: &mut Compilation) -> Result<Option<bool>> {
  self.sealing_hooks_report("chunk modules optimization", 13)?;
  Ok(None)
}

#[plugin_hook(CompilationModuleIds for ProgressPlugin)]
fn module_ids(&self, _modules: &mut Compilation) -> Result<()> {
  self.sealing_hooks_report("module ids", 16)
}

#[plugin_hook(CompilationChunkIds for ProgressPlugin)]
fn chunk_ids(&self, _compilation: &mut Compilation) -> Result<()> {
  self.sealing_hooks_report("chunk ids", 21)
}

#[plugin_hook(CompilationProcessAssets for ProgressPlugin, stage = Compilation::PROCESS_ASSETS_STAGE_ADDITIONAL)]
async fn process_assets(&self, _compilation: &mut Compilation) -> Result<()> {
  self.sealing_hooks_report("asset processing", 35)
}

#[plugin_hook(CompilationAfterProcessAssets for ProgressPlugin)]
async fn after_process_assets(&self, _compilation: &mut Compilation) -> Result<()> {
  self.sealing_hooks_report("after asset optimization", 36)
}

#[plugin_hook(CompilerEmit for ProgressPlugin)]
async fn emit(&self, _compilation: &mut Compilation) -> Result<()> {
  self.handler(0.98, "emitting".to_string(), vec!["emit".to_string()], None)
}

#[plugin_hook(CompilerAfterEmit for ProgressPlugin)]
async fn after_emit(&self, _compilation: &mut Compilation) -> Result<()> {
  self.handler(
    1.0,
    "emitting".to_string(),
    vec!["after emit".to_string()],
    None,
  )
}

#[async_trait]
impl Plugin for ProgressPlugin {
  fn name(&self) -> &'static str {
    "progress"
  }

  fn apply(&self, ctx: PluginContext<&mut ApplyContext>, _options: &CompilerOptions) -> Result<()> {
    ctx
      .context
      .compiler_hooks
      .this_compilation
      .tap(this_compilation::new(self));
    ctx
      .context
      .compiler_hooks
      .compilation
      .tap(compilation::new(self));
    ctx.context.compiler_hooks.make.tap(make::new(self));
    ctx
      .context
      .compilation_hooks
      .build_module
      .tap(build_module::new(self));
    ctx
      .context
      .compilation_hooks
      .succeed_module
      .tap(succeed_module::new(self));
    ctx
      .context
      .compiler_hooks
      .finish_make
      .tap(finish_make::new(self));
    ctx
      .context
      .compilation_hooks
      .finish_modules
      .tap(finish_modules::new(self));
    ctx.context.compilation_hooks.seal.tap(seal::new(self));
    ctx
      .context
      .compilation_hooks
      .optimize_dependencies
      .tap(optimize_dependencies::new(self));
    ctx
      .context
      .compilation_hooks
      .optimize_modules
      .tap(optimize_modules::new(self));
    ctx
      .context
      .compilation_hooks
      .after_optimize_modules
      .tap(after_optimize_modules::new(self));
    ctx
      .context
      .compilation_hooks
      .optimize_chunks
      .tap(optimize_chunks::new(self));
    ctx
      .context
      .compilation_hooks
      .optimize_tree
      .tap(optimize_tree::new(self));
    ctx
      .context
      .compilation_hooks
      .optimize_chunk_modules
      .tap(optimize_chunk_modules::new(self));
    ctx
      .context
      .compilation_hooks
      .module_ids
      .tap(module_ids::new(self));
    ctx
      .context
      .compilation_hooks
      .chunk_ids
      .tap(chunk_ids::new(self));
    ctx
      .context
      .compilation_hooks
      .process_assets
      .tap(process_assets::new(self));
    ctx
      .context
      .compilation_hooks
      .after_process_assets
      .tap(after_process_assets::new(self));
    ctx.context.compiler_hooks.emit.tap(emit::new(self));
    ctx
      .context
      .compiler_hooks
      .after_emit
      .tap(after_emit::new(self));
    Ok(())
  }
}
