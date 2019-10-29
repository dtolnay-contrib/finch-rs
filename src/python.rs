use std::collections::VecDeque;
use std::fmt::Display;
use std::fs::File;

use numpy::{PyArray, PyArray1, PyArray2};
use pyo3::class::*;
use pyo3::exceptions::{IndexError, KeyError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyBytes, PyList, PyTuple, PyType};
use pyo3::{create_exception, wrap_pyfunction};

use crate::distance::{best_distance, distance, distance_scaled, minmer_matrix};
use crate::filtering::FilterParams;
use crate::serialization::{write_finch_file, Sketch as SType};
use crate::sketch_schemes::{KmerCount, SketchParams};
use crate::{Result as FinchResult, sketch_files as rs_sketch_files, open_sketch_file};

create_exception!(finch, FinchError, pyo3::exceptions::Exception);

fn to_pyerr(e: impl Display) -> PyErr {
    PyErr::new::<FinchError, _>(format!("{}", e))
}

fn merge_sketches(sketch: &mut SType, other: &SType, size: Option<usize>) -> FinchResult<()> {
    // update my parameters from the remote's
    sketch.seq_length += other.seq_length;
    sketch.num_valid_kmers += other.num_valid_kmers;

    // TODO: do something with filters?
    // TODO: we should also check the sketch_params are compatible?

    // now merge the hashes together; someday it would be nice to use something idiomatic like:
    // https://users.rust-lang.org/t/solved-merge-multiple-sorted-vectors-using-iterators/6543
    let sketch1 = &sketch.hashes;
    let sketch2 = &other.hashes;

    let mut new_hashes = Vec::with_capacity(sketch1.len() + sketch2.len());
    let (mut i, mut j) = (0, 0);
    while (i < sketch1.len()) && (j < sketch2.len()) {
        if sketch1[i].hash < sketch2[j].hash {
            new_hashes.push(sketch1[i].clone());
            i += 1;
        } else if sketch2[j].hash < sketch1[i].hash {
            new_hashes.push(sketch2[j].clone());
            j += 1;
        } else {
            new_hashes.push(KmerCount {
                hash: sketch1[i].hash,
                kmer: sketch1[i].kmer.clone(),
                count: sketch1[i].count + sketch2[j].count,
                extra_count: sketch1[i].extra_count + sketch2[j].extra_count,
            });
            i += 1;
            j += 1;
        }
    }

    // now clip to the appropriate size
    let scale = sketch.sketch_params.hash_info().3;
    match (size, scale) {
        (Some(s), Some(sc)) => {
            let max_hash = u64::max_value() / (1. / sc) as u64;
            // truncate to hashes <= max/sc (or) s whichever is higher
            new_hashes = new_hashes
                .into_iter()
                .enumerate()
                .take_while(|(ix, h)| (h.hash <= max_hash) || (*ix < s))
                .map(|(_, h)| h)
                .collect();
        }
        (None, Some(sc)) => {
            let max_hash = u64::max_value() / (1. / sc) as u64;
            // truncate to hashes <= max/sc
            new_hashes = new_hashes
                .into_iter()
                .take_while(|h| h.hash <= max_hash)
                .collect();
        }
        (Some(s), None) => {
            // truncate to size
            new_hashes.truncate(s);
        }
        (None, None) => {
            // no filtering
        }
    }
    sketch.hashes = new_hashes;
    Ok(())
}

#[pyclass]
/// A Multisketch is a collection of Sketchs with information about their generation parameters
/// (to make sure they're consistant for distance calculation).
pub struct Multisketch {
    pub sketches: Vec<SType>,
}

#[pymethods]
impl Multisketch {
    #[classmethod]
    /// open(filename: str)
    ///
    /// Takes a file path to a `.sk`, `.bsk` or a `.mash` file and returns the Multisketch
    /// represented by that file.
    pub fn open(_cls: &PyType, filename: &str) -> PyResult<Multisketch> {
        Ok(Multisketch { sketches: open_sketch_file(filename).map_err(to_pyerr)? })
    }

    #[classmethod]
    /// from_sketches(sketches: List[Sketch])
    pub fn from_sketches(_cls: &PyType, sketches: &PyList) -> PyResult<Multisketch> {
        let sketches: Vec<&Sketch> = sketches.extract()?;
        let sketches = sketches.iter().map(|s| s.s.clone()).collect();
        Ok(Multisketch { sketches })
    }

    /// save(self, filename: str)
    ///
    /// Saves the collection of sketches to the filename provided
    pub fn save(&self, filename: &str) -> PyResult<()> {
        // TODO: support other file formats
        let mut out = File::create(&filename)
            .map_err(|_| PyErr::new::<FinchError, _>(format!("Could not create {}", filename)))?;
        write_finch_file(&mut out, &self.sketches).map_err(to_pyerr)?;
        Ok(())
    }

    /// best_match(sketch: Sketch)
    pub fn best_match(&self, query: &Sketch) -> PyResult<(usize, Sketch)> {
        // TODO: this should return an error if self.sketches is empty?
        let mut min_sketch: usize = 0;
        // since this is a query against a set of references we're using
        // 1 - containment as the comparison metric
        let mut min_dist: f64 = 1.;
        for (ix, sketch) in self.sketches.iter().enumerate() {
            // TODO: use best_distance here and elsewhere?
            let dist = distance_scaled(&sketch.hashes, &query.s.hashes, &sketch.name, &query.s.name).map_err(to_pyerr)?;
            if (1. - dist.containment) < min_dist {
                min_dist = 1. - dist.containment;
                min_sketch = ix;
            }
        }
        Ok((min_sketch, self.sketches[min_sketch].clone().into()))
    }

    // TODO: this is a little niche/untested; do we want this?
    pub fn squash(&self) -> PyResult<Sketch> {
        let mut sketch_iter = self.sketches.iter();
        let mut s = sketch_iter
            .next()
            .ok_or_else(|| PyErr::new::<FinchError, _>("No sketches to squash"))?
            .clone();
        let mut sketch_size = Some(s.sketch_params.expected_size());
        if sketch_size == Some(0) {
            sketch_size = None;
        }
        for sketch in sketch_iter {
            merge_sketches(&mut s, &sketch, sketch_size).map_err(to_pyerr)?;
        }
        Ok(s.into())
    }
}

#[pyproto]
impl PyIterProtocol for Multisketch {
    fn __iter__(slf: PyRefMut<Self>) -> PyResult<SketchIter> {
        let sketches = slf
            .sketches
            .iter()
            .map(|s| s.clone().into())
            .collect();
        Ok(SketchIter { sketches })
    }
}

#[pyclass]
pub struct SketchIter {
    sketches: VecDeque<Sketch>,
}

#[pyproto]
impl PyIterProtocol for SketchIter {
    fn __next__(mut slf: PyRefMut<Self>) -> PyResult<Option<Sketch>> {
        Ok(slf.sketches.pop_front())
    }
}

#[pyproto]
impl PyObjectProtocol for Multisketch {
    fn __repr__(&self) -> PyResult<String> {
        let n_sketches = self.sketches.len();
        let sketch_plural = if n_sketches == 1 {
            "sketch"
        } else {
            "sketches"
        };
        Ok(format!("<Multisketch ({} {})>", n_sketches, sketch_plural))
    }
}

#[inline]
fn _get_sketch_index(sketches: &[SType], key: &PyAny) -> PyResult<usize> {
    if let Ok(int_key) = key.extract::<isize>() {
        let l = sketches.len() as isize;
        if -l <= int_key && int_key < 0 {
            Ok((l - int_key) as usize)
        } else if 0 <= int_key && int_key < l {
            Ok(int_key as usize)
        } else {
            Err(PyErr::new::<IndexError, _>("index out of range"))
        }
    } else if let Ok(str_key) = key.extract::<&str>() {
        // TODO: we should maybe build an internal HashMap cache for this?
        // (note we have to handle non-unique keys then unless we want to
        // just standardize on returning the first matching item always)
        let remove_idx = sketches.iter().position(|s| s.name == str_key);
        if let Some(idx) = remove_idx {
            Ok(idx)
        } else {
            Err(PyErr::new::<KeyError, _>(str_key.to_string()))
        }
    } else {
        Err(PyErr::new::<FinchError, _>("key is not a string or integer"))
    }
}

#[pyproto]
impl PyMappingProtocol for Multisketch {
    fn __len__(&self) -> PyResult<usize> {
        Ok(self.sketches.len())
    }

    fn __getitem__(&self, key: &PyAny) -> PyResult<Sketch> {
        let idx = _get_sketch_index(&self.sketches, key)?;
        Ok(self.sketches[idx].clone().into())
    }

    fn __delitem__(&mut self, key: &PyAny) -> PyResult<()> {
        // TODO: if we ever allow sketches to just reference back to the
        // Multisketch this function could prove problematic?
        let idx = _get_sketch_index(&self.sketches, key)?;
        self.sketches.remove(idx);
        Ok(())
    }
}

// TODO: we need support for adding and removing sketches from the Multisketch

#[pyproto]
impl PySequenceProtocol for Multisketch {
    fn __contains__(&self, key: &str) -> PyResult<bool> {
        // TODO: also use the same cache as above?
        for sketch in &self.sketches {
            if sketch.name == key {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// A Sketch is a collection of deterministically-selected hashes from a single
/// sequencing file.
#[pyclass]
pub struct Sketch {
    pub s: SType,
}

#[pymethods]
impl Sketch {
    #[new]
    fn __new__(obj: &PyRawObject, name: &str) -> PyResult<()> {
        // TODO: take a hashes parameter: Vec<(usize, &[u8], u16, u16)>,
        // and a sketch_params?
        let sketch_params = SketchParams::Mash {
            kmers_to_sketch: 1000,
            final_size: 1000,
            no_strict: true,
            kmer_length: 21,
            hash_seed: 0,
        };
        let s = SType {
            name: name.to_string(),
            seq_length: 0,
            num_valid_kmers: 0,
            comment: String::new(),
            hashes: Vec::new(),
            sketch_params,
            filter_params: FilterParams::default(),
        };
        obj.init(Sketch { s });
        Ok(())
    }

    #[getter]
    fn get_name(&self) -> PyResult<String> {
        Ok(self.s.name.clone())
    }

    #[setter]
    fn set_name(&mut self, value: &str) -> PyResult<()> {
        self.s.name = value.to_string();
        Ok(())
    }

    #[getter]
    fn get_seq_length(&self) -> PyResult<u64> {
        Ok(self.s.seq_length)
    }

    #[getter]
    fn get_num_valid_kmers(&self) -> PyResult<u64> {
        Ok(self.s.num_valid_kmers)
    }

    #[getter]
    fn get_comment(&self) -> PyResult<String> {
        Ok(self.s.comment.clone())
    }

    #[setter]
    fn set_comment(&mut self, value: &str) -> PyResult<()> {
        self.s.comment = value.to_string();
        Ok(())
    }

    #[getter]
    fn get_hashes(&self) -> PyResult<Vec<(u64, PyObject, u32, u32)>> {
        let gil = Python::acquire_gil();
        let py = gil.python();
        self.s
            .hashes
            .clone()
            .into_iter()
            .map(|i| {
                Ok((
                    i.hash,
                    PyBytes::new(py, &i.kmer).into(),
                    i.count,
                    i.extra_count,
                ))
            })
            .collect()
    }

    #[getter]
    pub fn sketch_params(&self) -> PyResult<String> {
        // FIXME: the return format here is not great
        Ok(match self.s.sketch_params {
            SketchParams::Mash {
                kmers_to_sketch,
                final_size,
                no_strict,
                kmer_length,
                hash_seed,
            } => {
                let is_no_strict = if no_strict { "true" } else { "false" };
                format!(
                    "{{\"sketch_type\": \"mash\", \"kmers_to_sketch\": {}, \"final_size\": {}, \"no_strict\": {}, \"kmer_length\": {}, \"hash_seed\": {}}}",
                    kmers_to_sketch, final_size, is_no_strict, kmer_length, hash_seed,
                )
            }
            SketchParams::Scaled {
                kmers_to_sketch,
                kmer_length,
                scale,
                hash_seed,
            } => format!(
                "{{\"sketch_type\": \"scaled\", \"kmers_to_sketch\": {}, \"kmer_length\": {}, \"scale\": {}, \"hash_seed\": {}}}",
                kmers_to_sketch, kmer_length, scale, hash_seed,
            ),
            SketchParams::AllCounts { kmer_length } => format!(
                "{{\"sketch_type\": \"none\", \"kmer_length\": {}}}",
                kmer_length,
            ),
        })
    }

    // TODO: there are a lot of issues to fix in here; we should also try to destructure the
    // list depending on the format of the tuples; i.e. allow (usize, &[u8], u16, u16),
    // (usize, &[u8], u16), (usize, u16), (usize, &[u8]), usize, etc

    // #[setter]
    // fn set_hashes(&self, value: &PyObject) -> PyResult<()> {

    //     let value: &[(usize, PyBytes, u16, u16)] = PyObjectRef::extract(value)?;
    //     let kmers: Vec<KmerCount> = value.iter().map(|(hash, kmer, count, extra_count)| {
    //         KmerCount {
    //             hash: *hash,
    //             kmer: kmer.as_bytes().to_vec(),
    //             count: *count,
    //             extra_count: *extra_count,
    //         }
    //     }).collect();
    //     self.s.set_kmers(&kmers);
    //     Ok(())
    // }

    // TODO: filtering method

    // TODO: clip to n kmers/hashes method

    /// merge(sketch, size)
    ///
    /// Merge the second sketch into this one. If size is specified, use
    /// that as the new sketch's size. If scale is specified, merge the
    /// sketches together as if they are scaled sketches (for scaled sketches
    /// that have 'high' hashes because they're under `size`, this will
    /// potentially remove those hashes if the new sketch is large enough).
    pub fn merge(&mut self, sketch: &Sketch, size: Option<usize>) -> PyResult<()> {
        Ok(merge_sketches(&mut self.s, &sketch.s, size).map_err(to_pyerr)?)
    }

    /// compare(sketch, mash_mode=False)
    ///
    /// Calculates the containment within and jaccard similarity to another sketch.
    #[args(mash_mode = true)]
    pub fn compare(&self, sketch: &Sketch, mash_mode: bool) -> PyResult<(f64, f64)> {
        let dist =
            distance(&self.s.hashes, &sketch.s.hashes, &"", &"", mash_mode).map_err(to_pyerr)?;

        Ok((dist.containment, dist.jaccard))
    }

    /// compare_scaled(sketch)
    ///
    /// Calculates the containment within and jaccard similarity to another scaled sketch.
    pub fn compare_scaled(&self, sketch: &Sketch) -> PyResult<(f64, f64)> {
        let dist = distance_scaled(&self.s.hashes, &sketch.s.hashes, &"", &"").map_err(to_pyerr)?;

        Ok((dist.containment, dist.jaccard))
    }

    /// compare_counts(sketch)
    ///
    /// e.g.
    /// common, ref_pos, query_pos, ref_count, query_count, var, skew, kurt = db_sketch.compare_counts(query)
    pub fn compare_counts(
        &self,
        sketch: &Sketch,
    ) -> PyResult<(u64, u64, u64, u64, u64, f64, f64, f64)> {
        let reference = &self.s.hashes;
        let query = &sketch.s.hashes;
        let mut common: u64 = 0;
        let mut ref_pos: usize = 0;
        let mut ref_count: u64 = 0;
        let mut query_pos: usize = 0;
        let mut query_count: u64 = 0;
        // statistical moment calculation code derived from the example at:
        // https://en.wikipedia.org/wiki/Algorithms_for_calculating_variance#Higher-order_statistics
        let mut query_mean: f64 = 0.;
        let mut query_m2: f64 = 0.;
        let mut query_m3: f64 = 0.;
        let mut query_m4: f64 = 0.;

        while (ref_pos < reference.len()) && (query_pos < query.len()) {
            if reference[ref_pos].hash < query[query_pos].hash {
                ref_pos += 1;
            } else if query[query_pos].hash < reference[ref_pos].hash {
                query_pos += 1;
            } else {
                // bump counts
                ref_count += u64::from(reference[ref_pos].count);
                query_count += u64::from(query[query_pos].count);
                // bump query stats
                let n = common as f64 + 1.;
                let float_count = f64::from(query[query_pos].count);
                let delta: f64 = float_count - query_mean;
                let delta_n: f64 = delta / n;
                let delta_n2: f64 = delta_n * delta_n;
                let term1 = delta * delta_n * (n - 1.);

                query_mean += delta_n;
                query_m4 += term1 * delta_n2 * (n * n - 3. * n + 3.) + 6. * delta_n2 * query_m2
                    - 4. * delta_n * query_m3;
                query_m3 += term1 * delta_n * (n - 2.) - 3. * delta_n * query_m2;
                query_m2 += term1;

                // bump counters
                ref_pos += 1;
                query_pos += 1;
                common += 1;
            }
        }

        // mean is just (query_count / common) so we don't need to return it
        let var = query_m2 / common as f64;
        let skew = (common as f64).sqrt() * query_m3 / query_m2.powf(1.5);
        let kurt = (common as f64) * query_m4 / (query_m2 * query_m2) - 3.;

        Ok((
            common,
            ref_pos as u64,
            query_pos as u64,
            ref_count,
            query_count,
            var,
            skew,
            kurt,
        ))
    }

    /// compare_matrix(*sketches)
    ///
    /// Generates a numpy matrix of hash/kmer counts aligned to a "primary"
    /// reference. This matrix can then be used for downstream NNLS analysis.
    #[args(args = "*")]
    pub fn compare_matrix(&self, args: &PyTuple) -> PyResult<Py<PyArray2<u32>>> {
        let sketches: Vec<&Sketch> = args.extract()?;
        let sketch_kmers: Vec<&[KmerCount]> = sketches.iter().map(|s| &s.s.hashes[..]).collect();
        let result = minmer_matrix(&self.s.hashes, &sketch_kmers);

        let gil = Python::acquire_gil();
        let py = gil.python();
        Ok(PyArray::from_owned_array(py, result).to_owned())
    }

    #[getter]
    pub fn get_counts(&self) -> PyResult<Py<PyArray1<u32>>> {
        let result = self.s.hashes.iter().map(|k| k.count);

        let gil = Python::acquire_gil();
        let py = gil.python();
        Ok(PyArray::from_exact_iter(py, result).to_owned())
    }

    #[setter]
    pub fn set_counts(&mut self, value: &PyArray1<u32>) -> PyResult<()> {
        if value.len() != self.s.hashes.len() {
            return Err(PyErr::new::<FinchError, _>("counts must be same length as sketch"));
        }
        self.s.hashes = self.s.hashes.iter().zip(value.as_array().iter()).filter_map(|(s, v)| {
            if *v == 0 {
                None
            } else {
                let mut s = s.clone();
                s.count = *v;
                Some(s)
            }
        }).collect();

        Ok(())
    }

    pub fn copy(&self) -> PyResult<Sketch> {
        Ok(Sketch { s: self.s.clone() })
    }
}

#[pyproto]
impl PyObjectProtocol for Sketch {
    fn __repr__(&self) -> PyResult<String> {
        Ok(format!("<Sketch \"{}\">", self.s.name.clone()))
    }
}

#[pyproto]
impl PyMappingProtocol for Sketch {
    fn __len__(&self) -> PyResult<usize> {
        Ok(self.s.len())
    }
}

impl From<SType> for Sketch {
    fn from(s: SType) -> Self {
        Sketch { s }
    }
}

// TODO: impl PyNumberProtocol addition or subtraction for Sketch to allow merging/
// set difference calculations for sketches?
// see https://github.com/PyO3/pyo3/blob/master/tests/test_arithmetics.rs for details

// TODO: also it would be sweet to add a `str` to the Sketch to kmerize it and
// add the kmers

/// sketch_files(filenames, n_hashes, final_size, kmer_length, filter, seed)
/// ---
///
/// From the FASTA and FASTQ file paths, create a Multisketch.
// #[pyfunction(n_hashes=null, kmer_length=21, filter=true, seed=0)]  // TODO: this doesn't work?
#[pyfunction]
pub fn sketch_files(
    filenames: Vec<&str>,
    n_hashes: usize,
    final_size: Option<usize>,
    kmer_length: u8,
    filter: bool,
    seed: u64,
) -> PyResult<Multisketch> {
    // TODO: allow more filter customization?

    // TODO: allow passing in a single file without the list
    let sketch_params = SketchParams::Mash {
        kmers_to_sketch: n_hashes,
        final_size: final_size.unwrap_or(n_hashes),
        no_strict: false,
        kmer_length,
        hash_seed: seed,
    };
    let filters = FilterParams {
        filter_on: Some(filter),
        abun_filter: (None, None),
        err_filter: 1.,
        strand_filter: 0.1,
    };
    let sketches = rs_sketch_files(&filenames, &sketch_params, &filters).map_err(to_pyerr)?;
    Ok(Multisketch { sketches })
}

/// Finch is a MinHash sketch processing library.
#[pymodule]
fn finch(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<Multisketch>()?;
    m.add_class::<Sketch>()?;
    m.add_wrapped(wrap_pyfunction!(sketch_files))?;

    Ok(())
}