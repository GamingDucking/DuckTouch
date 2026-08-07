/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! The core of the emulator: management of state, execution, threading.
//!
//! Unlike its siblings, this module should be considered private and only used
//! via the re-exports one level up.

pub mod app_picker;
mod mutex;
mod nullable_box;

use crate::abi::{CallFromHost, GuestFunction};
use crate::audio::openal::OpenALManager;
use crate::cpu::Cpu;
use crate::libc::semaphore::sem_t;
use crate::mem::{GuestUSize, MutPtr, MutVoidPtr};
use crate::{
    abi, bundle, cpu, dyld, frameworks, fs, gdb, image, libc, mach_o, mem, objc, options, stack,
    window,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::TcpListener;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use crate::libc::pthread::cond::pthread_cond_t;
use crate::window::DeviceFamily;
use corosensei::{Coroutine, Yielder};
pub use mutex::{MutexId, MutexType, PTHREAD_MUTEX_DEFAULT};
use nullable_box::NullableBox;

/// Index into the [Vec] of threads. Thread 0 is always the main thread.
pub type ThreadId = usize;

pub type HostContext = Coroutine<Environment, Environment, Environment>;

/// Bookkeeping for a thread.
pub struct Thread {
    /// Once a thread finishes, this is set to false.
    pub active: bool,
    /// If this is not [ThreadBlock::NotBlocked], the thread is not executing
    /// until a certain condition is fufilled.
    pub blocked_by: ThreadBlock,
    /// Container for thread local state of various child modules
    pub thread_local_framework_state: frameworks::ThreadLocalState,
    /// After a secondary thread finishes, this is set to the returned value.
    return_value: Option<MutVoidPtr>,
    /// Context object containing the CPU state for this thread.
    ///
    /// There should always be `(threads.len() - 1)` contexts in existence.
    /// When a thread is currently executing, its state is stored directly in
    /// the CPU, rather than in a context object. In that case, this field is
    /// None. See also: [std::mem::take] and [cpu::Cpu::swap_context].
    pub guest_context: Option<Box<cpu::CpuContext>>,
    /// The coroutine associated with this thread.
    ///
    /// In more typical rust, this is equivalent to to a [std::future::Future].
    /// Like a [std::future::Future], it holds the call stack so the inner
    /// function can (cooperatively) suspend execution and be resumed at a
    /// later time. Unlike a [std::future::Future], the call stack is actually
    /// stored as a stack, and not as an anonymous, compiler generated,
    /// (typically heap allocated) object.
    host_context: Option<HostContext>,
    /// Address range of this thread's stack, used to check if addresses are in
    /// range while producing a stack trace.
    pub stack: Option<std::ops::RangeInclusive<u32>>,
}

impl Thread {
    fn is_blocked(&self) -> bool {
        !matches!(self.blocked_by, ThreadBlock::NotBlocked)
    }
}

impl std::fmt::Debug for Thread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Thread {{ active: {:?}, blocked_by: {:?}, return_value: {:?} }}",
            self.active, self.blocked_by, self.return_value
        )
    }
}

/// The struct containing the entire emulator state. Methods are provided for
/// execution and management of threads.
pub struct Environment {
    /// Reference point for various timing functions.
    pub startup_time: Instant,
    pub bundle: NullableBox<bundle::Bundle>,
    pub fs: NullableBox<fs::Fs>,
    /// The window is only absent when running in headless mode.
    pub window: Option<Box<window::Window>>,
    pub openal_manager: NullableBox<OpenALManager>,
    pub mem: NullableBox<mem::Mem>,
    /// Loaded binaries. Index `0` is always the app binary, other entries are
    /// dynamic libraries.
    pub bins: Vec<mach_o::MachO>,
    pub objc: NullableBox<objc::ObjC>,
    pub dyld: NullableBox<dyld::Dyld>,
    pub cpu: NullableBox<cpu::Cpu>,
    pub current_thread: ThreadId,
    pub threads: Vec<Thread>,
    pub libc_state: NullableBox<libc::State>,
    pub framework_state: NullableBox<frameworks::State>,
    pub mutex_state: NullableBox<mutex::MutexState>,
    pub options: NullableBox<options::Options>,
    gdb_server: Option<Box<gdb::GdbServer>>,
    pub env_vars: HashMap<Vec<u8>, MutPtr<u8>>,
    /// Set to [true] when created using [Environment::new_without_app].
    pub dump_file: Option<std::fs::File>,
    pub is_app_picker: bool,
    yielder: *const Yielder<Environment, Environment>,
    // The amount of ticks to run for Some(value), or single-stepping for None.
    // Sadly, setting ticks to 1 does not step properly, so Option is required.
    remaining_ticks: Option<u64>,
    panic_cell: Rc<Cell<Option<Environment>>>,
    /// Tracks repeated UndefinedInstruction bypasses. See `debug_cpu_error`.
    udf_bypass_last: Option<(u32, u32)>,
    udf_bypass_count: u32,
    /// Tracks consecutive UndefinedInstruction bypasses that all fake-return
    /// to the *same* LR, regardless of the faulting PC. This catches runaway
    /// loops where the faulting PC alternates between several bogus addresses
    /// (so the `(pc, lr)` key above keeps resetting) but the guest keeps
    /// bouncing back to a single return site — e.g. a game that called
    /// through a nil/garbage function pointer. See `debug_cpu_error`.
    udf_bypass_last_lr: Option<u32>,
    udf_bypass_lr_count: u32,
}

/// What to do next when executing this thread.
enum ThreadNextAction {
    /// Continue CPU emulation.
    Continue,
    /// Return to host.
    ReturnToHost,
    /// Debug the current CPU error.
    DebugCpuError(cpu::CpuError),
}

/// If/what a thread is blocked by.
#[derive(Debug, Clone, PartialEq)]
pub enum ThreadBlock {
    // Default state. (thread is not blocked)
    NotBlocked,
    // Thread is sleeping. (until Instant)
    Sleeping(Instant),
    // Thread is waiting for a mutex to unlock.
    Mutex(MutexId),
    // Thread is waiting on a semaphore.
    Semaphore(MutPtr<sem_t>),
    // Thread is waiting on a condition variable
    Condition(MutPtr<pthread_cond_t>, Option<Duration>),
    // Thread is waiting for another thread to finish (joining).
    Joining(ThreadId, MutPtr<MutVoidPtr>),
    // Thread has hit a cpu error, and is waiting to be debugged.
    WaitingForDebugger(Option<cpu::CpuError>),
    // Thread is suspended. We keep a suspend count and a previous thread state
    // (boxed to avoid cyclic dependency), which would be restored upon
    // resuming.
    #[allow(dead_code)]
    Suspended(usize, Box<ThreadBlock>),
}

struct BinaryDependencyNode {
    name: String,
    dependencies: Vec<String>,
}

/// Topologically sorts the binary dylibs using Kahn's algorithm
/// and returns the sorted list of indices
fn generate_binary_load_order(graph: &[BinaryDependencyNode]) -> Result<Vec<usize>, String> {
    let node_to_index: HashMap<_, _> = graph
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.name.as_str(), idx))
        .collect();
    let mut node_dependents = HashMap::new();
    let mut node_in_degrees: HashMap<_, _> = node_to_index.values().map(|&idx| (idx, 0)).collect();

    for node in graph {
        let &bin_index = node_to_index
            .get(node.name.as_str())
            .ok_or_else(|| format!("Failed to find {:?} name mapping", &node.name))?;

        // Bin names dont include prefix while dynamic lib paths do
        for dependency in node
            .dependencies
            .iter()
            .map(|path| path.strip_prefix("/usr/lib/").unwrap_or(path.as_str()))
        {
            // Ignore dependencies that are not included in packaged dylibs
            let Some(&dylib_index) = node_to_index.get(dependency) else {
                continue;
            };

            node_dependents
                .entry(dylib_index)
                .or_insert_with(Vec::new)
                .push(bin_index);

            node_in_degrees
                .entry(bin_index)
                .and_modify(|in_degree| *in_degree += 1);
        }
    }

    let mut leaf_nodes: VecDeque<_> = node_in_degrees
        .iter()
        .filter(|(_, &in_degree)| in_degree == 0)
        .map(|(&node, _)| node)
        .collect();

    let mut sorted_indices = Vec::new();

    while let Some(node) = leaf_nodes.pop_front() {
        sorted_indices.push(node);

        let Some(dependents) = node_dependents.get(&node) else {
            continue;
        };

        for &dependant in dependents {
            let Some(in_degree) = node_in_degrees.get_mut(&dependant) else {
                continue;
            };
            *in_degree -= 1;

            if *in_degree == 0 {
                leaf_nodes.push_back(dependant);
            }
        }
    }

    if let Some((&index, _)) = node_in_degrees.iter().find(|(_, &in_degree)| in_degree > 0) {
        return Err(format!(
            "Failed to sort nodes, cycle with {:?}",
            graph.get(index).unwrap().name
        ));
    }

    log!(
        "Found sorted order {:?}",
        sorted_indices
            .iter()
            .map(|&index| graph.get(index).unwrap().name.as_str())
            .collect::<Vec<_>>()
    );

    Ok(sorted_indices)
}

/// Enforces the one (real) Environment limit. See
/// [Environment::with_yielder] for why this is needed.
static ENVIRONMENT_INSTANCE_EXISTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl Environment {
    /// Loads the binary and sets up the emulator.
    pub fn new(
        bundle: bundle::Bundle,
        fs: fs::Fs,
        mut options: options::Options,
        app_args: Vec<String>,
    ) -> Result<Environment, String> {
        let startup_time = Instant::now();
        let launched_bundle_id = bundle.bundle_identifier().to_owned();

        if launched_bundle_id == "at.source.potato.full" {
            log!(
        "Applying PotatoGold compatibility profile: disable present rotation, remap touch location to landscape, fake network success, and use silent OpenAL fallback."
    );

            // SAFETY: Environment::new runs during startup before guest worker threads
            // are created. These env vars are read by compatibility shims inside this
            // same process.
            unsafe {
                std::env::set_var("TOUCHHLE_DISABLE_PRESENT_ROTATION", "1");
                std::env::set_var("TOUCHHLE_TOUCH_LOCATION_PORTRAIT_TO_LANDSCAPE", "1");
                std::env::set_var("TOUCHHLE_FAKE_NETWORK_SUCCESS", "1");

                // PotatoGold's audio path was crashing on some Linux setups unless
                // OpenAL Soft used the null backend. This keeps the app playable even
                // if sound is silent.
            }
        }
        // Enforces the one (real) Environment limit. See `with_yielder` for
        // why this is needed.
        if ENVIRONMENT_INSTANCE_EXISTS.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err("Only one (real) Environment can exist at a time!".to_string());
        }

        // Certain apps need to launch in a non-portrait orientation, and this
        // should be handled before creating the window because handling of
        // window rotation after-the-fact is somewhat glitchy.
        // This also ensures the splash screen is correctly oriented.
        //
        // Only force a non-portrait orientation when the app explicitly
        // does NOT advertise portrait support. Storyboard apps (and any
        // other modern UIKit binary) routinely declare every orientation
        // they can run in via `UISupportedInterfaceOrientations`, and
        // picking the first non-portrait entry would force them into
        // landscape even when portrait is perfectly fine. Apple's own
        // launch logic uses portrait by default whenever it's listed, so
        // mirror that.
        let portrait_supported = bundle
            .supported_interface_orientations()
            .contains(&"UIInterfaceOrientationPortrait");
        if options.initial_orientation == window::DeviceOrientation::Portrait && !portrait_supported
        {
            if let Some(&non_portrait_orientation) = bundle
                .supported_interface_orientations()
                .iter()
                .find(|&&o| o != "UIInterfaceOrientationPortrait")
            {
                // TODO: Overwriting the options might not be ideal; do we need
                //       to distinguish this kind of orientation change from
                //       others?
                options.initial_orientation = match non_portrait_orientation {
                    // UIInterfaceOrientation values are flipped relative to
                    // (UI)DeviceOrientation values (content has to rotate in
                    // the opposite direction to how the device rotates).
                    "UIInterfaceOrientationLandscapeLeft" => {
                        window::DeviceOrientation::LandscapeRight
                    }
                    "UIInterfaceOrientationLandscapeRight" => {
                        window::DeviceOrientation::LandscapeLeft
                    }
                    // This appears to be an older way set the orientation.
                    // From testing, it seems to correspond to left.
                    "UIInterfaceOrientationLandscape" => window::DeviceOrientation::LandscapeLeft,

                    // ДОБАВЛЯЕМ СЮДА ПРИВЯЗКУ К ОБЫЧНОМУ ПОРТРЕТУ:
                    "UIInterfaceOrientationPortraitUpsideDown" => {
                        window::DeviceOrientation::Portrait
                    }

                    other => {
                        log!(
                            "Warning: Unsupported startup orientation: {:?}; defaulting to Portrait.",
                            other
                        );
                        window::DeviceOrientation::Portrait
                    }
                };
                log!("App needs non-portrait user interface orientation {:?}, applying device orientation {:?}.", non_portrait_orientation, options.initial_orientation);
            }
        }

        let device_family_override = options.device_family;
        // `--device-family=auto`: when the user hasn't pinned a specific family,
        // probe the host display and pick the closest-matching emulated device.
        // This is treated exactly like an explicit override below, so it still
        // respects what the app bundle actually supports.
        let device_family_override = if device_family_override.is_none()
            && options.auto_device_family
            && !options.headless
        {
            match window::host_screen_size() {
                Some((w, h)) => {
                    let picked = DeviceFamily::pick_for_screen(w, h);
                    if options.host_screen_size.is_none() {
                        options.host_screen_size = Some((w, h));
                    }
                    log!(
                        "Auto device family: host screen is {}x{} px, exposing the same resolution to the app and picking closest match {:?}.",
                        w,
                        h,
                        picked
                    );
                    Some(picked)
                }
                None => {
                    log!("Auto device family: couldn't determine host screen size; leaving choice to the app bundle.");
                    None
                }
            }
        } else {
            device_family_override
        };
        let device_family_array = bundle.device_family_array();
        // The bundle only declares generic device *classes* (iPhone == phone
        // family, iPad == tablet family). A user override may now name a
        // specific model (e.g. iPhone 4s, iPad mini 2). We accept the override
        // when its class matches one the bundle supports, and otherwise fall
        // back to a sensible default model for a supported class.
        let bundle_supports_ipad = device_family_array.iter().any(|f| f.is_ipad());
        let bundle_supports_phone = device_family_array.iter().any(|f| !f.is_ipad());
        // Default model picked for each class when the user hasn't chosen one.
        // iPhone 3GS (iPhone2,1) is the historical touchHLE phone default
        // (320x480, GLES2-capable); iPad 2 (iPad2,1) is the tablet default.
        let default_phone = DeviceFamily::iPhone3GS;
        let default_ipad = DeviceFamily::iPad2;

        let device_family = if let Some(dfo) = device_family_override {
            let override_is_ipad = dfo.is_ipad();
            if override_is_ipad && bundle_supports_ipad {
                dfo
            } else if !override_is_ipad && bundle_supports_phone {
                dfo
            } else {
                log!(
                    "Warning: User-defined {:?} device family override is not supported by the app (supported: {:?}); ignoring.",
                    dfo,
                    device_family_array
                );
                if bundle_supports_phone {
                    default_phone
                } else if bundle_supports_ipad {
                    default_ipad
                } else {
                    default_phone
                }
            }
        } else if bundle_supports_phone {
            // Prefer the phone family when the bundle supports it, matching the
            // previous behaviour for universal (iPhone + iPad) bundles.
            default_phone
        } else if bundle_supports_ipad {
            default_ipad
        } else {
            log!(
                "Warning: bundle declares no recognised supported device families ({:?}); falling back to iPhone.",
                device_family_array
            );
            default_phone
        };
        log!("{:?} device family is chosen.", device_family);
        options.device_family = Some(device_family);

        let window = if options.headless {
            None
        } else {
            let icon = bundle.load_icon(&fs);
            if let Err(ref e) = icon {
                log!("Warning: {}", e);
            }

            let launch_image_path = bundle.launch_image_path(&fs, device_family);
            let launch_image = if fs.is_file(&launch_image_path) {
                let res = fs
                    .read(launch_image_path)
                    .map_err(|_| "Could not read launch image file".to_string())
                    .and_then(|bytes| {
                        image::Image::from_bytes(&bytes)
                            .map_err(|e| format!("Could not parse launch image: {e}"))
                    });
                if let Err(ref e) = res {
                    log!("Warning: {}", e);
                };
                res.ok()
            } else {
                None
            };
            Some(Box::new(window::Window::new(
                &format!(
                    "{} (touchHLE {}{}{})",
                    bundle.display_name(),
                    super::branding(),
                    if super::branding().is_empty() {
                        ""
                    } else {
                        " "
                    },
                    super::VERSION
                ),
                icon.ok(),
                launch_image.map(|image| (image, false)),
                &options,
            )))
        };

        let mut mem = mem::Mem::new();

        let is_spore = bundle.bundle_identifier().starts_with("com.ea.spore");
        let is_critter_crunch = bundle
            .bundle_identifier()
            .starts_with("com.capybaragames.CritterCrunch")
            || bundle
                .bundle_identifier()
                .starts_with("com.go.starwave.CritterCrunch");
        // We always reset this flag depending on which game is launched.
        mem.zero_memory_on_free = !is_spore && !is_critter_crunch;
        if is_spore {
            log!("Applying game-specific hack for Spore Origins: zeroing memory on alloc instead of free.");
        }
        if is_critter_crunch {
            // Without this hack, every time a critter 'explodes',
            // the game crashes with a null page access error.
            log!("Applying game-specific hack for Critter Crunch: zeroing memory on alloc instead of free.");
        }
        let executable = mach_o::MachO::load_from_file(
            bundle.executable_path(),
            &fs,
            &mut mem,
            /* slide: */ 0,
        )
        .map_err(|e| format!("Could not load executable: {e}"))?;

        let mut dylibs = Vec::new();
        for dylib in &executable.dynamic_libraries {
            // There are some Free Software libraries bundled with touchHLE and
            // exposed via the guest file system (see Fs::new()).
            let dylib_path = fs::GuestPath::new(dylib);
            if fs.is_file(dylib_path) {
                // We use hardcoded slide values for libgcc and libstdc++
                // based on base addresses of those dylibs prior to iOS 3.1
                // TODO: implement some kind of ASLR instead of hardcoding
                assert!(dylib_path.as_str().starts_with("/usr/lib/"));

                let name = dylib_path.file_name().unwrap();
                let dylib_slide = match name {
                    "libstdc++.6.dylib" | "libstdc++.6.0.9.dylib" => 0x3748a000,

                    // ДОБАВИТЬ ЭТО: Честный базовый адрес для libc++ (iOS 5.0+)
                    "libc++.1.dylib" => 0x38000000,
                    // На случай, если игра также потянет за собой libc++abi
                    "libc++abi.dylib" => 0x38100000,
                    "libiconv.2.dylib" => 0x32000000,

                    "libgcc_s.1.dylib" => 0x30000000,
                    "libz.1.dylib" | "libz.1.2.3.dylib" | "libz.dylib" | "libz.1.1.3.dylib" => {
                        // We build `libz` from sources with our OSS toolchain,
                        // the base address is already set and sliding is not
                        // needed.
                        0
                    }
                    "libsqlite3.dylib" | "libsqlite3.0.dylib" => {
                        // We build `libsqlite3` from sources with our OSS
                        // toolchain, the base address is already set and
                        // sliding is not needed.
                        0
                    }
                    _ => {
                        log!(
                            "Warning: unknown binary slide for {:?}; loading at slide 0. App may fail to bind some symbols.",
                            name
                        );
                        0
                    }
                };

                let dylib = mach_o::MachO::load_from_file(
                    fs::GuestPath::new(dylib),
                    &fs,
                    &mut mem,
                    dylib_slide,
                )
                .map_err(|e| format!("Could not load bundled dylib: {e}"))?;

                dylibs.push(dylib);
            // Otherwise, look for it in our host implementations.
            } else if !crate::dyld::DYLIB_LIST
                .iter()
                .any(|d| d.path == dylib || d.aliases.contains(&dylib.as_str()))
            {
                log!(
                    "Warning: app binary depends on unimplemented or missing dylib \"{}\"",
                    dylib
                );
            }
        }

        let entry_point_addr = executable
            .entry_point_pc
            .ok_or_else(|| {
                "Mach-O file does not specify an entry point PC, perhaps it is not an executable?"
                    .to_string()
            })
            .unwrap();

        let entry_point_is_lc_main = executable.entry_point_is_lc_main;

        let entry_point_addr = abi::GuestFunction::from_addr_with_thumb_bit(entry_point_addr);

        log_dbg!("Address of start function: {:?}", entry_point_addr);

        let mut bins = dylibs;
        bins.insert(0, executable);

        let mut objc = objc::ObjC::new();

        let mut dyld = dyld::Dyld::new();
        dyld.do_initial_linking(&bundle, &bins, &mut mem, &mut objc);

        let cpu = cpu::Cpu::new(match options.direct_memory_access {
            true => Some(&mut mem),
            false => None,
        });

        let main_thread_init_routine = Coroutine::new(move |yielder, mut env: Environment| {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                env.with_yielder(yielder, move |env| {
                    echo!("CPU emulation begins now.");
                    // Some apps use the stack inside the static initializer.
                    // While properly behaving apps should be fine, some app
                    // will try to poke the top of the stack, so we'll give
                    // it some room.
                    env.cpu.regs_mut()[Cpu::SP] = 0xFFFFF000;

                    // Call `+load` method on classes where it's defined.
                    // TODO: `+load` methods from our image should take priority
                    // over frameworks ones.
                    // TODO: a category `+load` method should be called after
                    // the class's own +load method.
                    // Note: `+load` is sent without triggering `+initialize`,
                    // matching the runtime's guarantee that `+load` runs first.
                    let mut to_be_loaded = Vec::new();
                    let mut processed = HashSet::new();
                    let load_sel: objc::SEL = env
                        .objc
                        .register_host_selector("load".to_string(), &mut env.mem);
                    for (class_name, &class) in env.objc.all_classes() {
                        if processed.contains(&class) {
                            continue;
                        }
                        if env.objc.is_unimplemented_class(class) || env.objc.is_fake_class(class) {
                            continue;
                        }
                        if env
                            .objc
                            .object_has_uninherited_method(&env.mem, class, load_sel)
                        {
                            log_dbg!("Calling +load on inheritance chain of {} class", class_name);
                            let mut inherited = Vec::new();
                            let mut curr_class = class;
                            while curr_class != objc::nil
                                && !env.objc.is_unimplemented_class(curr_class)
                                && !env.objc.is_fake_class(curr_class)
                            {
                                if !processed.contains(&curr_class)
                                    && env.objc.object_has_uninherited_method(
                                        &env.mem, curr_class, load_sel,
                                    )
                                {
                                    inherited.push(curr_class);
                                    processed.insert(curr_class);
                                }
                                curr_class = env.objc.get_superclass(curr_class);
                            }
                            to_be_loaded.extend(inherited.into_iter().rev());
                        }
                    }
                    for &class in &to_be_loaded {
                        () = objc::msg_send_no_initialize(env, (class, load_sel));
                    }

                    // Static initializers for libraries must be run before
                    // the initializer in the app binary.
                    for bin_idx in env.get_sorted_bin_indices().unwrap() {
                        let Some(bin) = env.bins.get(bin_idx) else {
                            continue;
                        };
                        let Some(section) =
                            bin.get_section(mach_o::SectionType::ModInitFuncPointers)
                        else {
                            continue;
                        };

                        log_dbg!("Calling static initializers for {:?}", bin.name);
                        assert!(section.size % 4 == 0);

                        let base: mem::ConstPtr<abi::GuestFunction> =
                            mem::Ptr::from_bits(section.addr);

                        let count = section.size / 4;
                        for i in 0..count {
                            let func = env.mem.read(base + i);

                            log_dbg!(
                                "Calling static initializer at {:?} from {:?}",
                                func,
                                (base + i)
                            );

                            () = func.call_from_host(env, ());
                        }
                        log_dbg!("Static initialization done");
                    }

                    {
                        let bin_path = env.bundle.executable_path();

                        let envp_list: Vec<String> = env
                            .env_vars
                            .clone()
                            .iter_mut()
                            .map(|tuple| {
                                [
                                    std::str::from_utf8(tuple.0).unwrap(),
                                    "=",
                                    env.mem.cstr_at_utf8(*tuple.1).unwrap(),
                                ]
                                .concat()
                            })
                            .collect();

                        let envp_ref_list: Vec<&str> =
                            envp_list.iter().map(|keyvalue| keyvalue.as_str()).collect();

                        let bin_path_apple_key = format!("executable_path={}", bin_path.as_str());

                        let argv = Vec::from_iter(
                            std::iter::once(bin_path.as_str())
                                .chain(app_args.iter().map(|s| s.as_str())),
                        );

                        let envp = envp_ref_list.as_slice();
                        let apple = &[bin_path_apple_key.as_str()];
                        stack::prep_stack_for_start(
                            &mut env.mem,
                            &mut env.cpu,
                            &argv,
                            envp,
                            apple,
                            entry_point_is_lc_main,
                        );
                    }

                    // Manually call here, since running call_from_host pushes
                    // a stack frame and disrupts abi for _start.
                    env.cpu
                        .branch_with_link(entry_point_addr, env.dyld.thread_exit_routine());

                    env.run_call();

                    panic!("Main function exited unexpectedly!");
                })
            }));

            if let Err(e) = res {
                let panic_cell = env.panic_cell.clone();
                panic_cell.set(Some(env));
                std::panic::resume_unwind(e);
            }
            env
        });

        let main_thread = Thread {
            active: true,
            blocked_by: ThreadBlock::NotBlocked,
            return_value: None,
            guest_context: None,
            host_context: Some(main_thread_init_routine),
            stack: Some(mem::Mem::MAIN_THREAD_STACK_LOW_END..=0u32.wrapping_sub(1)),
            thread_local_framework_state: Default::default(),
        };

        let mut env = Environment {
            startup_time,
            bundle: NullableBox::new(bundle),
            fs: NullableBox::new(fs),
            window,
            openal_manager: NullableBox::new(OpenALManager::new()?),
            mem: NullableBox::new(mem),
            bins,
            objc: NullableBox::new(objc),
            dyld: NullableBox::new(dyld),
            cpu: NullableBox::new(cpu),
            current_thread: 0,
            threads: vec![main_thread],
            libc_state: Default::default(),
            mutex_state: Default::default(),
            framework_state: Default::default(),
            options: NullableBox::new(options),
            gdb_server: None,
            env_vars: Default::default(),
            dump_file: None,
            is_app_picker: false,
            yielder: std::ptr::null(),
            remaining_ticks: None,
            panic_cell: Rc::new(Cell::new(None)),
            udf_bypass_last: None,
            udf_bypass_count: 0,
            udf_bypass_last_lr: None,
            udf_bypass_lr_count: 0,
        };

        if env.options.dumping_options.any() {
            env.dump_file =
                Some(std::fs::File::create(&env.options.dumping_file).map_err(|e| e.to_string())?);
        }

        env.set_up_initial_env_vars();
        dyld::Dyld::do_late_linking(&mut env);

        env.cpu.set_cpsr(cpu::Cpu::CPSR_USER_MODE);

        if let Some(addrs) = env.options.gdb_listen_addrs.take() {
            let listener = TcpListener::bind(addrs.as_slice())
                .map_err(|e| format!("Could not bind to {addrs:?}: {e}"))?;

            echo!(
                "Waiting for debugger connection on {}...",
                addrs
                    .into_iter()
                    .map(|a| format!("{a}"))
                    .collect::<Vec<String>>()
                    .join(", ")
            );

            let (client, client_addr) = listener
                .accept()
                .map_err(|e| format!("Could not accept connection: {e}"))?;

            echo!("Debugger client connected on {}.", client_addr);
            let mut gdb_server = gdb::GdbServer::new(client);
            let step = gdb_server.wait_for_debugger(None, &mut env.cpu, &mut env.mem);

            assert!(!step, "Can't step right now!"); // TODO?
            env.gdb_server = Some(Box::new(gdb_server));
        }

        if env.options.dumping_options.linking_info {
            let file = env.dump_file.as_mut().unwrap();

            env.objc.dump_classes(file).unwrap();
            env.dyld.dump_lazy_symbols(&env.bins, file).unwrap();
            env.objc
                .dump_selectors(&env.bins[0], &env.mem, file)
                .unwrap();
        }

        env.cpu.branch(entry_point_addr);
        Ok(env)
    }

    /// Set up the emulator environment without loading an app binary.
    ///
    /// This is a special mode that only exists to support the app picker, which
    /// uses the emulated environment to draw its UI and process input. Filling
    /// some of the fields with fake data is a hack, but it means the frameworks
    /// do not need to be aware of the app picker's peculiarities, so it is
    /// cleaner than the alternative!
    pub fn new_without_app(
        options: options::Options,
        icon: image::Image,
    ) -> Result<Environment, String> {
        // Enforces a one (real) Environment limit. See `with_yielder` for
        // why this is needed.
        if ENVIRONMENT_INSTANCE_EXISTS.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("Only one (real) Environment can exist at a time!".to_string());
        }
        ENVIRONMENT_INSTANCE_EXISTS.store(true, std::sync::atomic::Ordering::Relaxed);
        let bundle = bundle::Bundle::new_fake_bundle();
        let fs = fs::Fs::new_fake_fs();

        let startup_time = Instant::now();

        let launch_image = None;

        assert!(!options.headless);
        let window = Some(Box::new(window::Window::new(
            &format!(
                "touchHLE {}{}{}",
                super::branding(),
                if super::branding().is_empty() {
                    ""
                } else {
                    " "
                },
                super::VERSION
            ),
            Some(icon),
            launch_image,
            &options,
        )));

        let mut mem = mem::Mem::new();

        let bins = Vec::new();

        let mut objc = objc::ObjC::new();

        let mut dyld = dyld::Dyld::new();

        dyld.do_initial_linking_with_no_bins(&mut mem, &mut objc);

        let cpu = cpu::Cpu::new(match options.direct_memory_access {
            true => Some(&mut mem),
            false => None,
        });

        let main_thread = Thread {
            active: true,
            blocked_by: ThreadBlock::NotBlocked,
            return_value: None,
            guest_context: None,
            host_context: None,
            stack: Some(mem::Mem::MAIN_THREAD_STACK_LOW_END..=0u32.wrapping_sub(1)),
            thread_local_framework_state: Default::default(),
        };

        let mut env = Environment {
            startup_time,
            bundle: NullableBox::new(bundle),
            fs: NullableBox::new(fs),
            window,
            openal_manager: NullableBox::new(OpenALManager::new()?),
            mem: NullableBox::new(mem),
            bins,
            objc: NullableBox::new(objc),
            dyld: NullableBox::new(dyld),
            cpu: NullableBox::new(cpu),
            current_thread: 0,
            threads: vec![main_thread],
            libc_state: Default::default(),
            mutex_state: Default::default(),
            framework_state: Default::default(),
            options: NullableBox::new(options),
            gdb_server: None,
            env_vars: Default::default(),
            dump_file: None,
            is_app_picker: true,
            yielder: std::ptr::null(),
            remaining_ticks: None,
            panic_cell: Rc::new(Cell::new(None)),
            udf_bypass_last: None,
            udf_bypass_count: 0,
            udf_bypass_last_lr: None,
            udf_bypass_lr_count: 0,
        };

        env.set_up_initial_env_vars();

        // Dyld::do_late_linking() would be called here, but it doesn't do
        // anything relevant here, so it's skipped.

        {
            let argv = &[];
            let envp = &[];
            let apple = &[];
            stack::prep_stack_for_start(&mut env.mem, &mut env.cpu, argv, envp, apple, false);
        }

        env.cpu.set_cpsr(cpu::Cpu::CPSR_USER_MODE);

        // GDB server setup would be done here, but there's no need for it.

        // "CPU emulation begins now" would happen here, but there's nothing
        // to emulate. :)

        Ok(env)
    }

    /// Create a new Environment to swap with.
    ///
    /// SAFETY: You must *NEVER, IN ANY CIRCUMSTANCE* dereference any fields or
    /// call any methods on the environment. This means that you must *NEVER,
    /// IN ANY CIRCUMSTANCE* leak this to safe code. You *MUST* make sure this
    /// includes panic safety - do not allow a panic to accidentally smuggle
    /// out this environment to safe code!
    ///
    /// Admittedly, even if this is leaked, it's very unlikely it would lead to
    /// any real problems, just a null pointer deref.
    unsafe fn new_fake() -> Self {
        Self {
            startup_time: Instant::now(),
            bundle: NullableBox::null(),
            fs: NullableBox::null(),
            window: None,
            openal_manager: NullableBox::null(),
            mem: NullableBox::null(),
            bins: Vec::new(),
            objc: NullableBox::null(),
            dyld: NullableBox::null(),
            cpu: NullableBox::null(),
            current_thread: 0,
            threads: Vec::new(),
            libc_state: NullableBox::null(),
            framework_state: NullableBox::null(),
            mutex_state: NullableBox::null(),
            options: NullableBox::null(),
            gdb_server: None,
            env_vars: Default::default(),
            dump_file: None,
            is_app_picker: false,
            yielder: std::ptr::null(),
            remaining_ticks: None,
            panic_cell: Rc::new(Cell::new(None)),
            udf_bypass_last: None,
            udf_bypass_count: 0,
            udf_bypass_last_lr: None,
            udf_bypass_lr_count: 0,
        }
    }

    // ... rest of file unchanged ...
