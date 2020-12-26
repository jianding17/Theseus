// Copyright 2016 Philipp Oppermann. See the README.md
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use super::{Frame, FrameAllocator, FrameRange, PhysicalAddress, PhysicalMemoryArea};
use alloc::vec::Vec;
use kernel_config::memory::PAGE_SIZE;
use core::mem;
use core::ptr;

/// A stand-in for a Union
pub enum VectorArray<T: Clone> {
    Array((usize, [T; 32])),
    Vector(Vec<T>),
}
impl<T: Clone> VectorArray<T> {
    pub fn upgrade_to_vector(&mut self) {
        let new_val = { 
            match *self {
                VectorArray::Array((count, ref arr)) => { 
                    Some(VectorArray::Vector(arr[0..count].to_vec()))
                }
                _ => { 
                    None // no-op, it's already a Vector
                }
            }
        };
        if let Some(nv) = new_val {
            *self = nv;
        }
    }

    // pub fn iter(&self) -> ::core::slice::Iter<T> {
    //     match self {
    //         &VectorArray::Array((_count, arr)) => arr.iter(),
    //         &VectorArray::Vector(v) => v[0..v.len()].iter(),
    //     }
    // }

}




/// A frame allocator that uses the memory areas from the multiboot information structure as
/// source. The {kernel, multiboot}_{start, end} fields are used to avoid returning memory that is
/// already in use.
///
/// `kernel_end` and `multiboot_end` are _inclusive_ bounds.
/// # Arguments
/// * `freed_frame_list`: a statically allocated stack that stores frame numbers of deallocated frames.     
/// * `first_allocated_frame`: stores the fisrt frame that is allocated by the frame allocator. We need
/// *     to avoid re-allocate this frame because it is used by the P4 page table 
pub struct AreaFrameAllocator {
    next_free_frame: Frame,
    current_area: Option<PhysicalMemoryArea>,
    available: VectorArray<PhysicalMemoryArea>,
    occupied: VectorArray<PhysicalMemoryArea>,
    freed_frame_list: StaticArrayStack<usize>,
    first_allocated_frame: usize,
}

impl AreaFrameAllocator {
    pub fn new(
        available: [PhysicalMemoryArea; 32], 
        avail_len: usize, 
        occupied: [PhysicalMemoryArea; 32], 
        occ_len: usize
    ) -> Result<AreaFrameAllocator, &'static str> {
        let mut allocator = AreaFrameAllocator {
            next_free_frame: Frame::containing_address(PhysicalAddress::zero()),
            current_area: None,
            available: VectorArray::Array((avail_len, available)),
            occupied: VectorArray::Array((occ_len, occupied)),
            freed_frame_list: StaticArrayStack::new(),
            first_allocated_frame: 0,
        };
        allocator.select_next_area();
        Ok(allocator)
    }

    /// `available`: specifies whether the given `area` is an available or occupied memory area.
    pub fn add_area(&mut self, area: PhysicalMemoryArea, available: bool) -> Result<(), &'static str> {
        // match self.available {
        match if available { &mut self.available } else { &mut self.occupied } {
            &mut VectorArray::Array((ref mut count, ref mut arr)) => {
                if *count < arr.len() {
                    arr[*count] = area;
                    *count += 1;
                }
                else {
                    error!("AreaFrameAllocator::add_area(): {} array is already full!", if available { "available" } else { "occupied" } );
                    return Err("array is already full");
                }
            }
            &mut VectorArray::Vector(ref mut v) => {
                v.push(area);
            }
        }

        // debugging stuff below
        trace!("AreaFrameAllocator: updated {} area: =======================================", if available { "available" } else { "occupied" });
        match if available { &self.available } else { &self.occupied } {
            &VectorArray::Array((ref count, ref arr)) => {
                trace!("   Array[{}]: {:?}", count, arr);
            }
            & VectorArray::Vector(ref v) => {
                trace!("   Vector: {:?}", v);
            }
        }


        Ok(())
    }

    fn select_next_area(&mut self) {
        self.current_area = match self.available {
            VectorArray::Array((len, ref arr)) => {
                arr.iter().take(len)
                    .filter(|area| {
                        let address = area.base_addr + area.size_in_bytes - 1;
                        area.typ == 1 && Frame::containing_address(address) >= self.next_free_frame
                    })
                    .min_by_key(|area| area.base_addr).cloned()
            }
            VectorArray::Vector(ref v) => {
                v.iter()
                    .filter(|area| {
                        let address = area.base_addr + area.size_in_bytes - 1;
                        area.typ == 1 && Frame::containing_address(address) >= self.next_free_frame
                    })
                    .min_by_key(|area| area.base_addr).cloned()
            }
        };
        
            
        trace!("AreaFrameAllocator: selected next area {:?}", self.current_area);

        if let Some(area) = self.current_area {
            let start_frame = Frame::containing_address(area.base_addr);
            if self.next_free_frame < start_frame {
                self.next_free_frame = start_frame;
            }
        }
    }

    /// Determines whether or not the current `next_free_frame` is within any occupied memory area,
    /// and advances it to the start of the next free region after the occupied area.
    fn skip_occupied_frames(&mut self) {
        let mut rerun = false;
        match self.occupied {
            VectorArray::Array((len, ref arr)) => {
                for area in arr.iter().take(len) {
                    let start = Frame::containing_address(area.base_addr);
                    let end = Frame::containing_address(area.base_addr + area.size_in_bytes);
                    if self.next_free_frame >= start && self.next_free_frame <= end {
                        self.next_free_frame = end + 1; 
                        trace!("AreaFrameAllocator: skipping occupied area to next frame {:?}", self.next_free_frame);
                        rerun = true;
                        break;
                    }
                }
            }
            VectorArray::Vector(ref v) => {
                for area in v.iter() {
                    let start = Frame::containing_address(area.base_addr);
                    let end = Frame::containing_address(area.base_addr + area.size_in_bytes);
                    if self.next_free_frame >= start && self.next_free_frame <= end {
                        self.next_free_frame = end + 1; 
                        trace!("AreaFrameAllocator: skipping occupied area to next frame {:?}", self.next_free_frame);
                        rerun = true;
                        break;
                    }
                }
            }
        };
        
        // If we actually skipped an occupied area, then we need to rerun this again,
        // to ensure that we didn't skip into another occupied area.
        if rerun {
            self.skip_occupied_frames();
        }
    }

    /// Determines whether or not the current `frame` is within any occupied memory area
    fn in_occupided_area(&self, frame: Frame) -> bool {
        match self.occupied {
            VectorArray::Array((len, ref arr)) => {
                for area in arr.iter().take(len) {
                    let start = Frame::containing_address(area.base_addr);
                    let end = Frame::containing_address(area.base_addr + area.size_in_bytes);
                    if frame >= start && frame <= end {
                        // trace!("AreaFrameAllocator: deallocation ingore frame {:?} is in occupied area", frame.number);
                        return true;
                    }
                }
            }
            VectorArray::Vector(ref v) => {
                for area in v.iter() {
                    let start = Frame::containing_address(area.base_addr);
                    let end = Frame::containing_address(area.base_addr + area.size_in_bytes);
                    if frame >= start && frame <= end { 
                        // trace!("AreaFrameAllocator: deallocation ingore frame {:?} is in occupied area", frame.number);
                        return true;
                    }
                }
            }
        };
        return false;
    }
}

impl FrameAllocator for AreaFrameAllocator {

    fn allocate_frames(&mut self, num_frames: usize) -> Option<FrameRange> {
        if num_frames == 0 { return None; }

        // this is just a shitty way to get contiguous frames, since right now it's really easy to get them
        // it wastes the frames that are allocated 
        // When contiguous frames are desired, set `use_freed_frames` to false to avoid allocating frames from previously deallocated frames
        if let Some(first_frame) = self.allocate_frame(false) {
            let first_frame_paddr = first_frame.start_address();

            // here, we successfully got the first frame, so try to allocate the rest
            for i in 1..num_frames {
                if let Some(f) = self.allocate_frame(false) {
                    if f.start_address() == (first_frame_paddr + (i * PAGE_SIZE)) {
                        // still getting contiguous frames, so we're good
                        continue;
                    }
                    else {
                        // didn't get a contiguous frame, so let's try again
                        warn!("AreaFrameAllocator::allocate_frames(): could only alloc {}/{} contiguous frames (those are wasted), trying again!", i, num_frames);
                        return self.allocate_frames(num_frames);
                    }
                }
                else {
                    error!("Error: AreaFrameAllocator::allocate_frames(): couldn't allocate {} contiguous frames, out of memory!", num_frames);
                    return None;
                }
            }

            // here, we have allocated enough frames, and checked that they're all contiguous
            let last_frame = first_frame + (num_frames - 1); // -1 for inclusive bound. Parenthesis needed to avoid overflow.
            return Some(FrameRange::new(first_frame, last_frame));
        }

        error!("Error: AreaFrameAllocator::allocate_frames(): couldn't allocate {} contiguous frames, out of memory!", num_frames);
        None
    }


    /// Allocate a frame from either previously deallocated frames or next free frame in the available area
    fn allocate_frame(&mut self, use_freed_frames: bool) -> Option<Frame> {
        if use_freed_frames && self.freed_frame_list.len > 0 {
            let frame_number = self.freed_frame_list.pop_back().unwrap();
            debug!("allocate frame {:?} from freed list with {:?} elements", frame_number, self.freed_frame_list.len + 1);
            return Some(Frame { number: frame_number})
                
        } else if let Some(area) = self.current_area {
            // first, see if we need to skip beyond the current area (it may be already occupied)
            self.skip_occupied_frames();

            // "clone" the frame to return it if it's free. Frame doesn't
            // implement Clone, but we can construct an identical frame.
            let frame = Frame { number: self.next_free_frame.number };

            // the last frame of the current area
            let last_frame_in_current_area = {
                let address = area.base_addr + area.size_in_bytes - 1;
                Frame::containing_address(address)
            };

            if frame > last_frame_in_current_area {
                // all frames of current area are used, switch to next area
                self.select_next_area();
            } else {
                // debug!("allocate frame {:?}", frame.number);
                if self.first_allocated_frame == 0 {
                    self.first_allocated_frame = frame.number;
                }
                // frame is unused, increment `next_free_frame` and return it
                self.next_free_frame += 1;
                // trace!("AreaFrameAllocator: allocated frame {:?}", frame);
                return Some(frame);
            }
            // `frame` was not valid, try it again with the updated `next_free_frame`
            debug!("allocate frame from next area");
            self.allocate_frame(false)
        } else {
            error!("FATAL ERROR: AreaFrameAllocator: out of physical memory!!!");
            None // no free frames left
        }
    }

    
    /// Recycle a deallocated frame into freed_frame_list for future allocation 
    /// if the frame is not in occupied area and it is not the first frame being allocated
    /// which is used for page table recursive mapping
    fn deallocate_frame(&mut self, frame: Frame) {
        if !self.in_occupided_area(frame) && frame.number != self.first_allocated_frame {    
            if frame.number == self.next_free_frame.number {
                self.next_free_frame -= 1;
            } else {
                unsafe {self.freed_frame_list.push_back(frame.number)};
            }
            debug!("deallocate frame: {:?}, next free frame: {:?}, length of freed_frame_list: {:?}", frame.number, self.next_free_frame.number,  self.freed_frame_list.len);
        }
    }


    /// Call this when the kernel heap has been set up
    fn alloc_ready(&mut self) {
        self.available.upgrade_to_vector();
        self.occupied.upgrade_to_vector();
    }
}

/// A statically allocated stack implemented from array.
pub struct StaticArrayStack<T> {
    arr: [T; 128],
    len: usize,
}


impl<T> StaticArrayStack<T> {
    pub fn new() -> StaticArrayStack<T> {
        StaticArrayStack {
            arr: unsafe { mem::zeroed() },
            len: 0,
        }
    }
    /// Push the given `value` onto the end of the array.
    pub unsafe fn push_back(&mut self, value: T) {
        if self.len < self.arr.len() {
            ptr::write(self.arr.as_mut_ptr().offset(self.len as isize), value);
            self.len += 1;
        } else {
            warn!("Out of space in array with size {:?}, failed to insert {:?}th value.", self.arr.len(), self.len);
        }
    }

    /// Pop the value at the tail of the array.
    pub fn pop_back(&mut self) -> Option<T> {
        if self.len == 0 {
            None
        } else {
                self.len -= 1; 
                return Some(unsafe {ptr::read(self.arr.get(self.len).unwrap())})
        }

    }
}

