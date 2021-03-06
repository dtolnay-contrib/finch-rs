#[macro_use]
extern crate clap;
#[macro_use]
extern crate failure;
extern crate finch;
extern crate serde_json;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{stderr, stdout, Write};
use std::process::exit;

use clap::{App, AppSettings, Arg, ArgMatches, SubCommand};

use finch::distance::distance;
use finch::serialization::{
    write_finch_file, write_mash_file, MultiSketch, Sketch, SketchDistance, FINCH_BIN_EXT,
    FINCH_EXT, MASH_EXT,
};
use finch::statistics::{cardinality, hist};
use finch::{open_sketch_file, sketch_files, Result};

use finch::main_parsing::{
    add_filter_options, add_sketch_options, get_float_arg, get_int_arg, parse_filter_options,
    parse_sketch_options, update_sketch_params,
};

fn add_output_options<'a, 'b>(app: App<'a, 'b>) -> App<'a, 'b> {
    app.arg(
        Arg::with_name("output_file")
            .short("o")
            .long("output")
            .help("Output to this file")
            .takes_value(true),
    )
    .arg(
        Arg::with_name("std_out")
            .short("O")
            .long("std-out")
            .help("Output to stdout ('print to terminal')")
            .conflicts_with("output_file"),
    )
}

fn output_to<F>(output_fn: F, output: Option<&str>, extension: &str) -> Result<()>
where
    F: Fn(&mut dyn Write) -> Result<()>,
{
    match output {
        None => {
            let mut out = stdout();
            output_fn(&mut out)?;
        }
        Some(o) => {
            // if the filename doesn't have the right extension
            // add it on
            let filename = String::from(o);
            let out_filename = if filename.ends_with(extension) {
                filename
            } else {
                filename + extension
            };

            let mut out = File::create(&out_filename)
                .map_err(|_| format_err!("Could not create {}", out_filename))?;
            output_fn(&mut out)?;
        }
    };
    Ok(())
}

fn main() {
    // see https://github.com/rust-lang-nursery/failure/issues/76
    if let Err(err) = run() {
        let mut serr = stderr();
        let mut causes = err.iter_chain();
        writeln!(
            serr,
            "Error: {}",
            causes.next().expect("`causes` to at least contain `err`")
        )
        .expect("unable to write error to stderr");
        for cause in causes {
            writeln!(serr, "Caused by: {}", cause).expect("unable to write error to stderr");
        }
        // The following assumes an `Error`, use `if let Some(backtrace) ...` for a `Fail`
        write!(serr, "{:?}", err.backtrace()).expect("unable to write error to stderr");
        exit(1);
    }
}

fn run() -> Result<()> {
    let mut sketch_command = SubCommand::with_name("sketch")
        .about("Create sketches from FASTA/Q file(s)")
        .arg(
            Arg::with_name("INPUT")
                .help("The file(s) to sketch")
                .multiple(true)
                .required(true),
        )
        .arg(
            Arg::with_name("binary_format")
                .short("b")
                .long("finch-binary-format")
                .help("Outputs sketch to a finch-native binary format"),
        )
        .arg(
            Arg::with_name("mash_binary_format")
                .short("B")
                .long("mash-binary-format")
                .conflicts_with("binary_format")
                .help("Outputs sketch in a binary format compatible with `mash`"),
        );
    sketch_command = add_output_options(sketch_command);
    sketch_command = add_filter_options(sketch_command);
    sketch_command = add_sketch_options(sketch_command);

    let mut dist_command = SubCommand::with_name("dist")
        .about("Compute distances between sketches")
        .arg(
            Arg::with_name("INPUT")
                .help("Sketchfile(s) to make comparisons for")
                .multiple(true)
                .required(true),
        )
        .arg(
            Arg::with_name("pairwise")
                .short("p")
                .long("pairwise")
                .conflicts_with("queries")
                .help("Calculate distances between all sketches"),
        )
        .arg(
            Arg::with_name("queries")
                .short("q")
                .long("queries")
                .help("All distances are from these sketches (sketches must be in the first file)")
                .multiple(true)
                .conflicts_with("pairwise")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("max_distance")
                .short("d")
                .long("max-dist")
                .help("Only report distances under this threshold")
                .default_value("1.0")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("old_dist_mode")
                .long("old-dist")
                .help("Calculate distances using the old containment-biased Finch mode"),
        );
    dist_command = add_output_options(dist_command);
    dist_command = add_filter_options(dist_command);
    dist_command = add_sketch_options(dist_command);

    let mut hist_command = SubCommand::with_name("hist")
        .about("Display histograms of kmer abundances")
        .arg(
            Arg::with_name("INPUT")
                .help("Generate histograms from these file(s)")
                .multiple(true)
                .required(true),
        );
    hist_command = add_output_options(hist_command);
    hist_command = add_filter_options(hist_command);
    hist_command = add_sketch_options(hist_command);

    let mut info_command = SubCommand::with_name("info")
        .about("Display basic statistics")
        .arg(
            Arg::with_name("INPUT")
                .help("Return stats on these file(s)")
                .multiple(true)
                .required(true),
        );
    info_command = add_output_options(info_command);
    info_command = add_filter_options(info_command);
    info_command = add_sketch_options(info_command);

    let matches = App::new("finch")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Tool for working with genomic MinHash sketches")
        .setting(AppSettings::VersionlessSubcommands)
        .setting(AppSettings::ArgRequiredElseHelp)
        .subcommand(sketch_command)
        .subcommand(dist_command)
        .subcommand(hist_command)
        .subcommand(info_command)
        .get_matches();

    if let Some(matches) = matches.subcommand_matches("sketch") {
        let file_ext = if matches.is_present("binary_format") {
            FINCH_BIN_EXT
        } else if matches.is_present("mash_binary_format") {
            MASH_EXT
        } else {
            FINCH_EXT
        };
        if matches.is_present("output_file") || matches.is_present("std_out") {
            let sketches = parse_mash_files(matches)?;
            let output = matches.value_of("output_file");

            output_to(
                |writer| {
                    if matches.is_present("binary_format") {
                        write_finch_file(writer, &sketches)?;
                    } else if matches.is_present("mash_binary_format") {
                        write_mash_file(writer, &sketches)?;
                    } else {
                        let multisketch = MultiSketch::from_sketches(&sketches)?;
                        serde_json::to_writer(writer, &multisketch)?;
                    }
                    Ok(())
                },
                output,
                &file_ext,
            )?;
        } else {
            // special case for "sketching in place"
            generate_sketch_files(matches, file_ext)?;
        }
    } else if let Some(matches) = matches.subcommand_matches("dist") {
        let old_mode = matches.is_present("old_dist_mode");

        let max_dist = get_float_arg(matches, "max_distance", 1f64)?;
        let all_sketches = parse_mash_files(matches)?;

        let mut query_sketches = Vec::new();
        if matches.is_present("pairwise") {
            for sketch in &all_sketches {
                query_sketches.push(sketch);
            }
        } else if matches.is_present("queries") {
            let query_names: HashSet<String> = matches
                .values_of("queries")
                .unwrap() // we already know it's present
                .map(|s| s.to_string())
                .collect();

            for sketch in &all_sketches {
                if query_names.contains(&sketch.name) {
                    query_sketches.push(&sketch);
                }
            }
        } else {
            if all_sketches.is_empty() {
                bail!("No sketches present!");
            }
            query_sketches.push(all_sketches.first().unwrap());
        }

        let distances = calc_sketch_distances(&query_sketches, &all_sketches, old_mode, max_dist);

        output_to(
            |writer| {
                serde_json::to_writer(writer, &distances)
                    .map_err(|_| format_err!("Could not serialize JSON to file"))?;
                Ok(())
            },
            matches.value_of("output_file"),
            ".json",
        )?;
    } else if let Some(matches) = matches.subcommand_matches("hist") {
        let mut hist_map: HashMap<String, Vec<u64>> = HashMap::new();
        let multisketch = parse_mash_files(matches)?;

        for sketch in multisketch {
            hist_map.insert(sketch.name.to_string(), hist(&sketch.hashes));
        }

        output_to(
            |writer| {
                serde_json::to_writer(writer, &hist_map)
                    .map_err(|_| format_err!("Could not serialize JSON to file"))?;
                Ok(())
            },
            matches.value_of("output_file"),
            ".json",
        )?;
    } else if let Some(matches) = matches.subcommand_matches("info") {
        // TODO: this should probably output JSON
        let multisketch = parse_mash_files(matches)?;

        for sketch in multisketch {
            print!("{}", &sketch.name);
            println!(" (from {}bp)", sketch.seq_length);
            let kmers = &sketch.hashes;
            if let Ok(c) = cardinality(kmers) {
                println!("  Estimated # of Unique Kmers: {}", c);
            }

            let histogram = hist(kmers);
            let mean = histogram
                .iter()
                .enumerate()
                .map(|(i, v)| ((i as f32 + 1f32) * *v as f32, *v as f32))
                .fold((0f32, 0f32), |e, s| (e.0 + s.0, e.1 + s.1));
            println!("  Estimated Average Depth: {}x", mean.0 / mean.1);

            let mut total_gc: u64 = 0;
            for kmer in kmers {
                total_gc += kmer
                    .kmer
                    .iter()
                    .map(|b| match *b {
                        b'G' | b'g' | b'C' | b'c' => u64::from(kmer.count),
                        _ => 0,
                    })
                    .sum::<u64>();
            }
            let total_bases = if kmers.is_empty() {
                0f32
            } else {
                mean.0 * kmers[0].kmer.len() as f32
            };
            println!(
                "  Estimated % GC: {}%",
                100f32 * total_gc as f32 / total_bases
            );
        }
    }
    Ok(())
}

fn generate_sketch_files(matches: &ArgMatches, file_ext: &str) -> Result<()> {
    let filenames: Vec<_> = matches
        .values_of("INPUT")
        .ok_or_else(|| format_err!("Bad INPUT"))?
        .collect();

    let kmer_length: u8 = get_int_arg(matches, "kmer_length")?;
    let filters = parse_filter_options(matches, kmer_length)?;
    let sketch_params = parse_sketch_options(matches, kmer_length, filters.filter_on)?;

    for filename in filenames {
        if filename.ends_with(".json")
            || filename.ends_with(FINCH_EXT)
            || filename.ends_with(FINCH_BIN_EXT)
            || filename.ends_with(MASH_EXT)
        {
            bail!("Filename {} is not a sequence file?", filename);
        }

        let sketches = sketch_files(&[filename], &sketch_params, &filters)?;

        let out_filename = filename.to_string() + file_ext;
        let mut out = File::create(&out_filename)
            .map_err(|_| format_err!("Could not open {}", out_filename))?;
        if matches.is_present("binary_format") {
            write_finch_file(&mut out, &sketches)?;
        } else if matches.is_present("mash_binary_format") {
            write_mash_file(&mut out, &sketches)?;
        } else {
            let multisketch: MultiSketch = MultiSketch::from_sketches(&sketches)?;
            serde_json::to_writer(&mut out, &multisketch)?;
        }
    }
    Ok(())
}

fn parse_mash_files(matches: &ArgMatches) -> Result<Vec<Sketch>> {
    let filenames: Vec<_> = matches
        .values_of("INPUT")
        .ok_or_else(|| format_err!("Bad INPUT"))?
        .collect();

    let mut sketch_filenames = Vec::new();
    let mut seq_filenames = Vec::new();
    for filename in filenames {
        if filename.ends_with(".json")
            || filename.ends_with(FINCH_EXT)
            || filename.ends_with(FINCH_BIN_EXT)
            || filename.ends_with(MASH_EXT)
        {
            sketch_filenames.push(filename);
        } else {
            seq_filenames.push(filename);
        }
    }

    let kmer_length: u8 = get_int_arg(matches, "kmer_length")?;
    let mut filters = parse_filter_options(matches, kmer_length)?;
    let mut sketch_params = parse_sketch_options(matches, kmer_length, filters.filter_on)?;

    let mut filename_iter = sketch_filenames.iter();
    if let Some(first_filename) = filename_iter.next() {
        let mut sketches = open_sketch_file(first_filename)?;

        update_sketch_params(matches, &mut sketch_params, &sketches[0], first_filename)?;
        // we also have to handle updating filter options separately because
        // kmer_length changes how we calculate the `err_filter`
        if matches.occurrences_of("kmer_length") == 0 {
            filters = parse_filter_options(matches, sketch_params.k())?;
        }

        // now do the filtering for the first sketch file
        if filters.filter_on == Some(true) {
            for mut sketch in &mut sketches {
                filters.filter_sketch(&mut sketch);
            }
        }

        // and then handle the rest of the sketch files
        for filename in filename_iter {
            let extra_sketches = open_sketch_file(filename)?;
            // check new sketches are compatible with original file
            for sketch in &extra_sketches {
                if let Some((name, v1, v2)) =
                    sketch_params.check_compatibility(&sketch.sketch_params)
                {
                    bail!(
                        "Sketch {} has {} {}, but working value is {}",
                        sketch.name,
                        name,
                        v2,
                        v1,
                    );
                }
            }
            sketches.extend(extra_sketches);
            if filters.filter_on == Some(true) {
                for mut sketch in &mut sketches {
                    filters.filter_sketch(&mut sketch);
                }
            }
        }

        // now handle the sequences
        let extra_sketches = sketch_files(&seq_filenames, &sketch_params, &filters)?;
        sketches.extend(extra_sketches);
        Ok(sketches)
    } else {
        // now handle the sequences
        sketch_files(&seq_filenames, &sketch_params, &filters)
    }
}

fn calc_sketch_distances(
    query_sketches: &[&Sketch],
    ref_sketches: &[Sketch],
    old_mode: bool,
    max_distance: f64,
) -> Vec<SketchDistance> {
    let mut distances = Vec::new();
    for ref_sketch in ref_sketches {
        for query_sketch in query_sketches {
            if query_sketch == &ref_sketch {
                continue;
            }
            let distance = distance(&query_sketch, &ref_sketch, old_mode).unwrap();
            if distance.mash_distance <= max_distance {
                distances.push(distance);
            }
        }
    }
    distances
}
