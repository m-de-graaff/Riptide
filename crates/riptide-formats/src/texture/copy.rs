//! Copies of one subresource between plain rows and the console layout.

use super::{Error, PlaneLayout, Shape};

/// How the rows of a plain ("linear") buffer are spaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LinearPitch {
    /// Bytes from the start of one row to the start of the next. At least
    /// the bytes of one row of the level.
    pub row: usize,
    /// Bytes from the start of one depth layer to the start of the next.
    /// Only read when the subresource has more than one depth layer, and
    /// then it must hold all of a layer's rows.
    pub depth: usize,
}

impl LinearPitch {
    /// Rows packed with no padding, layers `rows` rows apart: the pitch of a
    /// buffer that holds exactly the data.
    pub fn packed(row_bytes: usize, rows: usize) -> LinearPitch {
        LinearPitch {
            row: row_bytes,
            depth: row_bytes.saturating_mul(rows),
        }
    }
}

/// A run of elements that are neighbours in a row and also neighbours in
/// console memory, so that they can be copied in one piece. Offsets are from
/// the row's base in each buffer.
struct Run {
    tiled: usize,
    linear: usize,
    len: usize,
}

/// Everything one copy needs, checked against both buffers.
struct Plan<'a> {
    layout: &'a PlaneLayout,
    mip: usize,
    first_layer: u32,
    layers: u32,
    rows: u32,
    pitch: LinearPitch,
    runs: Vec<Run>,
}

impl Plan<'_> {
    /// The base of row `y` of layer `z` in console memory and in the plain
    /// buffer.
    fn row(&self, z: u32, y: u32) -> (usize, usize) {
        let level = &self.layout.mips()[self.mip];
        let (base, inner) = self.layout.store_of(level, self.first_layer + z);
        let tiled = base + self.layout.row_part(level, y, inner);
        let linear = z as usize * self.pitch.depth + y as usize * self.pitch.row;
        // The plan was checked against the buffer lengths, which are sizes
        // of slices in memory, so the offset fits.
        (tiled as usize, linear)
    }
}

impl PlaneLayout {
    /// How many bytes a plain buffer must hold for one subresource of level
    /// `mip` with rows spaced as `pitch` says: up to the end of the last
    /// row's data, without padding after it.
    ///
    /// Fails with [`Error::OutOfRange`] past the last level and with
    /// [`Error::TooSmall`] when a pitch is smaller than what it must hold or
    /// the size does not fit in memory.
    pub fn linear_bytes(&self, mip: u32, pitch: LinearPitch) -> Result<usize, Error> {
        let level = self.mip(mip).ok_or(Error::OutOfRange)?;
        let row_bytes = level.width as usize * self.element_bytes() as usize;
        if pitch.row < row_bytes {
            return Err(Error::TooSmall);
        }
        let layer_bytes = (level.height as usize - 1)
            .checked_mul(pitch.row)
            .and_then(|bytes| bytes.checked_add(row_bytes))
            .ok_or(Error::TooSmall)?;
        if level.depth == 1 {
            return Ok(layer_bytes);
        }
        // Layers must not overlap one another.
        if pitch.depth < layer_bytes {
            return Err(Error::TooSmall);
        }
        (level.depth as usize - 1)
            .checked_mul(pitch.depth)
            .and_then(|bytes| bytes.checked_add(layer_bytes))
            .ok_or(Error::TooSmall)
    }

    /// Checks one copy and finds the runs every row of it repeats.
    fn plan(
        &self,
        mip: u32,
        slice: u32,
        pitch: LinearPitch,
        tiled_len: usize,
        linear_len: usize,
    ) -> Result<Plan<'_>, Error> {
        let level = self.mip(mip).ok_or(Error::OutOfRange)?;
        if slice >= self.subresource_slices() {
            return Err(Error::OutOfRange);
        }
        if !self.is_addressable() {
            return Err(Error::Unsupported);
        }
        let need = self.linear_bytes(mip, pitch)?;
        let tiled_need = usize::try_from(self.bytes_from_mip(mip)).map_err(|_| Error::TooSmall)?;
        if linear_len < need || tiled_len < tiled_need {
            return Err(Error::TooSmall);
        }

        // A row's address is the row's part plus the column's part, so the
        // columns that are neighbours in memory are the same in every row.
        let element = self.element_bytes() as usize;
        let mut runs: Vec<Run> = Vec::new();
        for x in 0..level.width {
            let tiled = self.column_part(level, x) as usize;
            let linear = x as usize * element;
            match runs.last_mut() {
                Some(run) if run.tiled + run.len == tiled => run.len += element,
                _ => runs.push(Run {
                    tiled,
                    linear,
                    len: element,
                }),
            }
        }
        Ok(Plan {
            layout: self,
            mip: mip as usize,
            first_layer: if self.desc().shape == Shape::ThreeD {
                0
            } else {
                slice
            },
            layers: level.depth,
            rows: level.height,
            pitch,
            runs,
        })
    }

    /// Copies one subresource from plain rows into the console layout.
    ///
    /// `tiled` is the plane's memory from its start. It must hold at least
    /// [`PlaneLayout::bytes_from_mip`] of `mip`, which is the whole plane
    /// except for the smaller levels of a single-slice texture. `linear`
    /// holds the rows of level `mip` of array slice `slice`, spaced as
    /// `pitch` says. For a volume `slice` must be 0 and `linear` holds every
    /// depth layer of the level. Bytes of `tiled` that belong to no element
    /// of the subresource are left as they were.
    pub fn tile(
        &self,
        tiled: &mut [u8],
        mip: u32,
        slice: u32,
        linear: &[u8],
        pitch: LinearPitch,
    ) -> Result<(), Error> {
        let plan = self.plan(mip, slice, pitch, tiled.len(), linear.len())?;
        for z in 0..plan.layers {
            for y in 0..plan.rows {
                let (to, from) = plan.row(z, y);
                for run in &plan.runs {
                    tiled[to + run.tiled..][..run.len]
                        .copy_from_slice(&linear[from + run.linear..][..run.len]);
                }
            }
        }
        Ok(())
    }

    /// Copies one subresource out of the console layout into plain rows: the
    /// reverse of [`PlaneLayout::tile`], with the same arguments. Bytes of
    /// `linear` between the rows are left as they were.
    pub fn detile(
        &self,
        tiled: &[u8],
        mip: u32,
        slice: u32,
        linear: &mut [u8],
        pitch: LinearPitch,
    ) -> Result<(), Error> {
        let plan = self.plan(mip, slice, pitch, tiled.len(), linear.len())?;
        for z in 0..plan.layers {
            for y in 0..plan.rows {
                let (from, to) = plan.row(z, y);
                for run in &plan.runs {
                    linear[to + run.linear..][..run.len]
                        .copy_from_slice(&tiled[from + run.tiled..][..run.len]);
                }
            }
        }
        Ok(())
    }
}
