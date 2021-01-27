mod convert;
mod io;
mod reconstruction;

use std::convert::TryFrom;
use std::env;
use std::fs;
use std::panic;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use log::{error, info, warn};
use rayon::prelude::*;
use splashsurf_lib::nalgebra::Vector3;
use splashsurf_lib::AxisAlignedBoundingBox3d;
use structopt::StructOpt;

// TODO: Use different logging when processing multiple files in parallel
// TODO: Add start and end index for input file sequences
// TODO: Does coarse_prof work with multiple threads?
// TODO: Check if all paths supplied using the cmd args are valid
// TODO: Clean up the parameter structs and conversions

#[derive(Clone, Debug, StructOpt)]
#[structopt(
    name = "splashsurf",
    author = "Fabian Löschner <loeschner@cs.rwth-aachen.de>",
    about = "Surface reconstruction for particle data from SPH simulations (https://github.com/w1th0utnam3/splashsurf)"
)]
struct CommandlineArgs {
    /// Enable quiet mode (no output except for severe panic messages), overrides verbosity level
    #[structopt(long, short = "-q")]
    quiet: bool,
    /// Print more verbose output, use multiple "v"s for even more verbose output (-v, -vv)
    #[structopt(short, parse(from_occurrences))]
    verbosity: u64,
    /// Subcommands
    #[structopt(subcommand)]
    subcommand: Subcommand,
}

#[derive(Clone, Debug, StructOpt)]
enum Subcommand {
    /// Reconstruct a surface from particle data
    Reconstruct(ReconstructSubcommandArgs),
    /// Convert between particle formats (supports the same input and output formats as the reconstruction)
    Convert(convert::ConvertSubcommandArgs),
}

#[derive(Clone, Debug, StructOpt)]
struct ReconstructSubcommandArgs {
    /// Path to the input file where the particle positions are stored (supported formats: VTK, binary f32 XYZ, PLY, BGEO)
    #[structopt(short = "-i", parse(from_os_str))]
    input_file: PathBuf,
    /// Filename for writing the reconstructed surface to disk (default: "[original_filename]_surface.vtk")
    #[structopt(short = "-o", parse(from_os_str))]
    output_file: Option<PathBuf>,
    /// Optional base directory for all output files (default: current working directory)
    #[structopt(long, parse(from_os_str))]
    output_dir: Option<PathBuf>,
    /// The particle radius of the input data
    #[structopt(long)]
    particle_radius: f64,
    /// The rest density of the fluid
    #[structopt(long, default_value = "1000.0")]
    rest_density: f64,
    /// The kernel radius (more specifically its compact support radius) used for building the density map in multiplies of the particle radius
    #[structopt(long)]
    kernel_radius: f64,
    /// If a particle has no neighbors in this radius (in multiplies of the particle radius) it is considered as a free particle
    #[structopt(long)]
    splash_detection_radius: Option<f64>,
    /// The marching cubes grid size in multiplies of the particle radius
    #[structopt(long)]
    cube_size: f64,
    /// The iso-surface threshold for the density, i.e. value of the reconstructed density that indicates the fluid surface
    #[structopt(long)]
    surface_threshold: f64,
    /// Whether to enable the use of double precision for all computations (disabled by default)
    #[structopt(short = "-d")]
    use_double_precision: bool,
    /// Lower corner of the domain where surface reconstruction should be performed, format: domain-min=x_min;y_min;z_min (requires domain-max to be specified)
    #[structopt(
        long,
        number_of_values = 3,
        value_delimiter = ";",
        requires = "domain-max"
    )]
    domain_min: Option<Vec<f64>>,
    /// Upper corner of the domain where surface reconstruction should be performed, format:domain-max=x_max;y_max;z_max (requires domain-min to be specified)
    #[structopt(
        long,
        number_of_values = 3,
        value_delimiter = ";",
        requires = "domain-min"
    )]
    domain_max: Option<Vec<f64>>,
    /// Whether to disable spatial decomposition using an octree and use a global approach instead (slower)
    #[structopt(long)]
    no_octree: bool,
    /// Whether to disable stitching of the disjoint subdomain meshes when spatial decomposition is enabled (faster but does not result in manifold meshes)
    #[structopt(long)]
    no_stitching: bool,
    /// The maximum number of particles for leaf nodes of the octree, default is to compute it based on number of threads and particles
    #[structopt(long)]
    octree_max_particles: Option<usize>,
    /// Safety factor applied to the kernel radius when it's used as a margin to collect ghost particles in the leaf nodes
    #[structopt(long)]
    octree_ghost_margin_factor: Option<f64>,
    /// Optional filename for writing the point cloud representation of the intermediate density map to disk
    #[structopt(long, parse(from_os_str))]
    output_dm_points: Option<PathBuf>,
    /// Optional filename for writing the grid representation of the intermediate density map to disk
    #[structopt(long, parse(from_os_str))]
    output_dm_grid: Option<PathBuf>,
    /// Optional filename for writing the octree used to partition the particles to disk
    #[structopt(long, parse(from_os_str))]
    output_octree: Option<PathBuf>,
    /// Flag to enable multi-threading to process multiple input files in parallel, conflicts with --mt-particles
    #[structopt(long = "mt-files", conflicts_with = "parallelize-over-particles")]
    parallelize_over_files: bool,
    /// Flag to enable multi-threading for a single input file by processing chunks of particles in parallel, conflicts with --mt-files
    #[structopt(long = "mt-particles", conflicts_with = "parallelize-over-files")]
    parallelize_over_particles: bool,
    /// Set the number of threads for the worker thread pool
    #[structopt(long, short = "-n")]
    num_threads: Option<usize>,
}

#[derive(Copy, Clone, Debug)]
enum VerbosityLevel {
    None,
    Verbose,
    VeryVerbose,
}

impl From<u64> for VerbosityLevel {
    fn from(value: u64) -> Self {
        match value {
            0 => VerbosityLevel::None,
            1 => VerbosityLevel::Verbose,
            2 => VerbosityLevel::VeryVerbose,
            _ => VerbosityLevel::VeryVerbose,
        }
    }
}

impl VerbosityLevel {
    /// Maps this verbosity level to a log filter
    fn into_filter(self) -> Option<log::LevelFilter> {
        match self {
            VerbosityLevel::None => None,
            VerbosityLevel::Verbose => Some(log::LevelFilter::Debug),
            VerbosityLevel::VeryVerbose => Some(log::LevelFilter::Trace),
        }
    }
}

fn run_splashsurf() -> Result<(), anyhow::Error> {
    let cmd_args = CommandlineArgs::from_args();

    let verbosity = VerbosityLevel::from(cmd_args.verbosity);
    let is_quiet = cmd_args.quiet;

    initialize_logging(verbosity, is_quiet).context("Failed to initialize logging")?;
    log_program_info();

    match &cmd_args.subcommand {
        Subcommand::Reconstruct(cmd_args) => reconstruct_subcommand(cmd_args)?,
        Subcommand::Convert(cmd_args) => convert::convert_subcommand(cmd_args)?,
    }

    coarse_prof_write_string()?
        .split("\n")
        .filter(|l| l.len() > 0)
        .for_each(|l| info!("{}", l));

    Ok(())
}

fn main() -> Result<(), anyhow::Error> {
    /*
    // Panic hook for easier debugging
    panic::set_hook(Box::new(|panic_info| {
        println!("Panic occurred: {}", panic_info);
        println!("Add breakpoint here for debugging.");
    }));
    */

    std::process::exit(match run_splashsurf() {
        Ok(_) => 0,
        Err(err) => {
            log_error(&err);
            1
        }
    });
}

/// Prints an anyhow error and its full error chain using the log::error macro
fn log_error(err: &anyhow::Error) {
    error!("Error occurred: {}", err);
    err.chain()
        .skip(1)
        .for_each(|cause| error!("  caused by: {}", cause));
}

/// Executes the `reconstruct` subcommand
fn reconstruct_subcommand(cmd_args: &ReconstructSubcommandArgs) -> Result<(), anyhow::Error> {
    let paths = ReconstructionRunnerPathCollection::try_from(cmd_args)
        .context("Failed parsing input file path(s) from command line")?
        .collect();
    let args = ReconstructionRunnerArgs::try_from(cmd_args)
        .context("Failed processing parameters from command line")?;

    let result = if cmd_args.parallelize_over_files {
        paths.par_iter().try_for_each(|path| {
            reconstruction::entry_point(path, &args)
                .with_context(|| {
                    format!(
                        "Error while processing input file '{}' from a file sequence",
                        path.input_file.display()
                    )
                })
                .map_err(|err| {
                    // Already log the error in case there are multiple errors
                    log_error(&err);
                    err
                })
        })
    } else {
        paths
            .iter()
            .try_for_each(|path| reconstruction::entry_point(path, &args))
    };

    if result.is_ok() {
        info!("Successfully finished processing all inputs.");
    }

    result
}

/// All arguments that can be supplied to the surface reconstruction tool converted to useful types
pub struct ReconstructionRunnerArgs {
    params: splashsurf_lib::Parameters<f64>,
    use_double_precision: bool,
    io_params: io::FormatParameters,
}

// Convert raw command line arguments to more useful types
impl TryFrom<&ReconstructSubcommandArgs> for ReconstructionRunnerArgs {
    type Error = anyhow::Error;

    fn try_from(args: &ReconstructSubcommandArgs) -> Result<Self, Self::Error> {
        // Convert domain args to aabb
        let domain_aabb = match (&args.domain_min, &args.domain_max) {
            (Some(domain_min), Some(domain_max)) => {
                // This should already be ensured by StructOpt parsing
                assert_eq!(domain_min.len(), 3);
                assert_eq!(domain_max.len(), 3);

                // TODO: Check that domain_min < domain_max
                let to_na_vec = |v: &Vec<f64>| -> Vector3<f64> { Vector3::new(v[0], v[1], v[2]) };

                Some(AxisAlignedBoundingBox3d::new(
                    to_na_vec(domain_min),
                    to_na_vec(domain_max),
                ))
            }
            _ => None,
        };

        // Scale kernel radius and cube size by particle radius
        let kernel_radius = args.particle_radius * args.kernel_radius;
        let splash_detection_radius = args
            .splash_detection_radius
            .map(|r| args.particle_radius * r);
        let cube_size = args.particle_radius * args.cube_size;

        let spatial_decomposition = if args.no_octree {
            None
        } else {
            let subdivision_criterion = if let Some(max_particles) = args.octree_max_particles {
                splashsurf_lib::SubdivisionCriterion::MaxParticleCount(max_particles)
            } else {
                splashsurf_lib::SubdivisionCriterion::MaxParticleCountAuto
            };
            let ghost_particle_safety_factor = args.octree_ghost_margin_factor;
            let enable_stitching = !args.no_stitching;

            Some(splashsurf_lib::SpatialDecompositionParameters {
                subdivision_criterion,
                ghost_particle_safety_factor,
                enable_stitching,
            })
        };

        // Assemble all parameters for the surface reconstruction
        let params = splashsurf_lib::Parameters {
            particle_radius: args.particle_radius,
            rest_density: args.rest_density,
            kernel_radius,
            splash_detection_radius,
            cube_size,
            iso_surface_threshold: args.surface_threshold,
            domain_aabb,
            enable_multi_threading: args.parallelize_over_particles,
            spatial_decomposition,
        };

        // Optionally initialize thread pool
        if let Some(num_threads) = args.num_threads {
            splashsurf_lib::initialize_thread_pool(num_threads)?;
        }

        Ok(ReconstructionRunnerArgs {
            params,
            use_double_precision: args.use_double_precision,
            io_params: io::FormatParameters::default(),
        })
    }
}

#[derive(Clone, Debug)]
struct ReconstructionRunnerPathCollection {
    is_sequence: bool,
    input_file: PathBuf,
    output_file: PathBuf,
    output_density_map_points_file: Option<PathBuf>,
    output_density_map_grid_file: Option<PathBuf>,
    output_octree_file: Option<PathBuf>,
}

impl ReconstructionRunnerPathCollection {
    fn try_new<P: Into<PathBuf>>(
        is_sequence: bool,
        input_file: P,
        output_base_path: Option<P>,
        output_file: P,
        output_density_map_points_file: Option<P>,
        output_density_map_grid_file: Option<P>,
        output_octree_file: Option<P>,
    ) -> Result<Self, anyhow::Error> {
        let input_file = input_file.into();
        let output_base_path = output_base_path.map(|p| p.into());
        let output_file = output_file.into();
        let output_density_map_points_file = output_density_map_points_file.map(|p| p.into());
        let output_density_map_grid_file = output_density_map_grid_file.map(|p| p.into());
        let output_octree_file = output_octree_file.map(|p| p.into());

        if let Some(output_base_path) = output_base_path {
            let output_file = output_base_path.join(output_file);

            // Ensure that output directory exists/create it
            if let Some(output_dir) = output_file.parent() {
                if !output_dir.exists() {
                    info!("The output directory '{}' of the output file '{}' does not exist. Trying to create it now...", output_dir.display(), output_file.display());
                    fs::create_dir_all(output_dir).with_context(|| {
                        format!(
                            "Unable to create output directory '{}'",
                            output_dir.display()
                        )
                    })?;
                }
            }

            Ok(Self {
                is_sequence,
                input_file,
                output_file,
                output_density_map_points_file: output_density_map_points_file
                    .map(|f| output_base_path.join(f)),
                output_density_map_grid_file: output_density_map_grid_file
                    .map(|f| output_base_path.join(f)),
                output_octree_file: output_octree_file.map(|f| output_base_path.join(f)),
            })
        } else {
            Ok(Self {
                is_sequence,
                input_file,
                output_file,
                output_density_map_points_file,
                output_density_map_grid_file,
                output_octree_file,
            })
        }
    }

    /// Returns an input/output file path struct for each input file (basically one task per input file)
    fn collect(&self) -> Vec<ReconstructionRunnerPaths> {
        if self.is_sequence {
            let input_file = &self.input_file;
            let output_file = &self.output_file;

            let input_dir = input_file.parent().unwrap();
            let output_dir = output_file.parent().unwrap();

            let input_filename = input_file.file_name().unwrap().to_string_lossy();
            let output_filename = output_file.file_name().unwrap().to_string_lossy();

            let mut paths = Vec::new();
            let mut i: usize = 1;
            loop {
                let input_filename_i = input_filename.replace("{}", &i.to_string());
                let input_file_i = input_dir.join(input_filename_i);

                if input_file_i.is_file() {
                    let output_filename_i = output_filename.replace("{}", &i.to_string());
                    let output_file_i = output_dir.join(output_filename_i);

                    paths.push(ReconstructionRunnerPaths::new(
                        input_file_i,
                        output_file_i,
                        // Don't write density maps etc. when processing a sequence of files
                        None,
                        None,
                        None,
                    ));
                } else {
                    break;
                }

                i += 1;
            }

            paths
        } else {
            vec![
                ReconstructionRunnerPaths::new(
                    self.input_file.clone(),
                    self.output_file.clone(),
                    self.output_density_map_points_file.clone(),
                    self.output_density_map_grid_file.clone(),
                    self.output_octree_file.clone(),
                );
                1
            ]
        }
    }
}

// Convert input file command line arguments to internal representation
impl TryFrom<&ReconstructSubcommandArgs> for ReconstructionRunnerPathCollection {
    type Error = anyhow::Error;

    fn try_from(args: &ReconstructSubcommandArgs) -> Result<Self, Self::Error> {
        let output_suffix = "surface";

        // If the input file exists, a single input file should be processed
        if args.input_file.is_file() {
            // Use the user defined output file name if provided...
            let output_file = if let Some(output_file) = &args.output_file {
                output_file.clone()
            // ...otherwise, generate one based on the input filename
            } else {
                let input_stem = args.input_file.file_stem().unwrap().to_string_lossy();
                format!("{}_{}.vtk", input_stem, output_suffix).into()
            };

            Self::try_new(
                false,
                args.input_file.clone(),
                args.output_dir.clone(),
                output_file,
                args.output_dm_points.clone(),
                args.output_dm_grid.clone(),
                args.output_octree.clone(),
            )
        // If the input file does not exist, its possible that a sequence of files should be processed
        } else {
            warn!("The input file '{}' does not exist. Assuming this is a pattern for a sequence of files.", args.input_file.display());

            // Make sure that the supposed sequence pattern ends with a filename (and not with a path separator)
            let input_filename = match args.input_file.file_name() {
                Some(input_filename) => input_filename.to_string_lossy(),
                None => {
                    return Err(anyhow!(
                        "The input file path '{}' does not end with a filename",
                        args.input_file.display()
                    ))
                }
            };

            // Make sure that the parent directory of the sequence pattern exists
            if let Some(input_dir) = args.input_file.parent() {
                if !input_dir.is_dir() && input_dir != Path::new("") {
                    return Err(anyhow!(
                        "The parent directory '{}' of the input file path '{}' does not exist",
                        input_dir.display(),
                        args.input_file.display()
                    ));
                }
            }

            // Make sure that we have a placeholder '{}' in the filename part of the sequence pattern
            if input_filename.contains("{}") {
                let input_stem = args.input_file.file_stem().unwrap().to_string_lossy();
                // Currently, only VTK files are supported for output
                let output_filename = format!(
                    "{}.vtk",
                    input_stem.replace("{}", &format!("{}_{{}}", output_suffix))
                );

                Self::try_new(
                    true,
                    args.input_file.clone(),
                    args.output_dir.clone(),
                    output_filename.into(),
                    args.output_dm_points.clone(),
                    args.output_dm_grid.clone(),
                    args.output_octree.clone(),
                )
            } else {
                return Err(anyhow!(
                    "Input file does not exist or invalid file sequence pattern in input file path '{}'",
                    args.input_file.display()
                ));
            }
        }
    }
}

/// All file paths that are relevant for running a single surface reconstruction task
#[derive(Clone, Debug)]
pub(crate) struct ReconstructionRunnerPaths {
    pub input_file: PathBuf,
    pub output_file: PathBuf,
    pub output_density_map_points_file: Option<PathBuf>,
    pub output_density_map_grid_file: Option<PathBuf>,
    pub output_octree_file: Option<PathBuf>,
}

impl ReconstructionRunnerPaths {
    fn new(
        input_file: PathBuf,
        output_file: PathBuf,
        output_density_map_points_file: Option<PathBuf>,
        output_density_map_grid_file: Option<PathBuf>,
        output_octree_file: Option<PathBuf>,
    ) -> Self {
        ReconstructionRunnerPaths {
            input_file,
            output_file,
            output_density_map_points_file,
            output_density_map_grid_file,
            output_octree_file,
        }
    }
}

/// Initializes logging with fern
fn initialize_logging(verbosity: VerbosityLevel, quiet_mode: bool) -> Result<(), anyhow::Error> {
    let mut unknown_log_filter_level = None;
    let log_filter_level = if quiet_mode {
        // First option: disable logging in quiet mode
        log::LevelFilter::Off
    } else {
        // Second option: use verbosity level
        verbosity.into_filter().unwrap_or_else(|| {
            // Third option: use log level from env
            if let Some(log_level) = std::env::var_os("RUST_LOG") {
                let log_level = log_level.to_string_lossy().to_ascii_lowercase();
                match log_level.as_str() {
                    "off" => log::LevelFilter::Off,
                    "error" => log::LevelFilter::Error,
                    "warn" => log::LevelFilter::Warn,
                    "info" => log::LevelFilter::Info,
                    "debug" => log::LevelFilter::Debug,
                    "trace" => log::LevelFilter::Trace,
                    _ => {
                        unknown_log_filter_level = Some(log_level);
                        log::LevelFilter::Info
                    }
                }
            } else {
                // Fourth option: use default level
                log::LevelFilter::Info
            }
        })
    };

    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}][{}] {}",
                chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(log_filter_level)
        .chain(std::io::stdout())
        .apply()
        .map_err(|e| anyhow!("Unable to apply logger configuration ({:?})", e))?;

    if let Some(filter_level) = unknown_log_filter_level {
        error!(
            "Unknown log filter level '{}' defined in 'RUST_LOG' env variable, using INFO instead.",
            filter_level
        );
    }

    Ok(())
}

/// Prints program name, version etc. and command line arguments to log
fn log_program_info() {
    info!(
        "{} v{} ({})",
        env!("CARGO_BIN_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_NAME")
    );

    let cmd_line: String = {
        let mut cmd_line = String::new();
        for arg in env::args() {
            cmd_line.push_str(&arg);
            cmd_line.push(' ');
        }
        cmd_line.pop();
        cmd_line
    };
    info!("Called with command line: {}", cmd_line);
}

/// Returns the coarse_prof::write output as a string
fn coarse_prof_write_string() -> Result<String, anyhow::Error> {
    let mut buffer = Vec::new();
    splashsurf_lib::coarse_prof::write(&mut buffer)?;
    Ok(String::from_utf8_lossy(buffer.as_slice()).into_owned())
}
