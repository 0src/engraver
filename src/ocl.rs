extern crate aligned_alloc;
extern crate ocl_core as core;
extern crate page_size;

use self::core::{
    ArgVal, ContextProperties, DeviceInfo, Event, KernelWorkGroupInfo, PlatformInfo, Status,
};
use hasher::SafeCVoid;
use libc::{c_void, size_t, uint64_t};
use std::sync::mpsc::{channel, Sender};

const NONCE_SIZE: u64 = (2 << 17);

//use config::Cfg;
use plotter::Buffer;
use std::ffi::CString;

use std::mem::transmute;
use std::process;
use std::slice::from_raw_parts;
use std::sync::{Arc, Mutex};
use std::u64;

static SRC: &'static str = include_str!("ocl/kernel.cl");

// convert the info or error to a string for printing:
macro_rules! to_string {
    ($expr:expr) => {
        match $expr {
            Ok(info) => info.to_string(),
            Err(err) => match err.api_status() {
                Some(Status::CL_KERNEL_ARG_INFO_NOT_AVAILABLE) => "Not available".into(),
                _ => err.to_string(),
            },
        }
    };
}

pub fn platform_info() {
    let platform_ids = core::get_platform_ids().unwrap();
    for (i, platform_id) in platform_ids.iter().enumerate() {
        println!(
            "OCL: platform {}, {} - {}",
            i,
            to_string!(core::get_platform_info(&platform_id, PlatformInfo::Name)),
            to_string!(core::get_platform_info(&platform_id, PlatformInfo::Version))
        );
        let device_ids = core::get_device_ids(&platform_id, None, None).unwrap();
        for (j, device_id) in device_ids.iter().enumerate() {
            println!(
                "OCL: device {}, {} - {}",
                j,
                to_string!(core::get_device_info(device_id, DeviceInfo::Vendor)),
                to_string!(core::get_device_info(device_id, DeviceInfo::Name))
            );
        }
    }
}

pub struct GpuContext {
    context: core::Context,
    queue: core::CommandQueue,
    kernel1: core::Kernel,
    ldim1: [usize; 3],
    gdim1: [usize; 3],
    mapping: bool,
    buffer_host_a: Vec<u8>,
    //  buffer_host_b: Vec<u8>,
    buffer_gpu_a: core::Mem,
    buffer_gpu_b: core::Mem,
    pub worksize: usize,
}

pub fn gpu_init(gpus: &Vec<String>, quiet: bool) -> Vec<Arc<GpuContext>> {
    let mut result = Vec::new();
    for gpu in gpus.iter() {
        let gpu = gpu.split(":").collect::<Vec<&str>>();
        let platform_id = gpu[0].parse::<usize>().unwrap();
        let gpu_id = gpu[1].parse::<usize>().unwrap();

        let platform_ids = core::get_platform_ids().unwrap();
        if platform_id >= platform_ids.len() {
            println!("Error: Selected OpenCL platform doesn't exist.");
            println!("Shutting down...");
            process::exit(0);
        }
        let platform = platform_ids[platform_id];
        let device_ids = core::get_device_ids(&platform, None, None).unwrap();
        if gpu_id >= device_ids.len() {
            println!("Error: Selected OpenCL device doesn't exist");
            println!("Shutting down...");
            process::exit(0);
        }
        let device = device_ids[gpu_id];
        let mut total_mem = 0;
        match core::get_device_info(&device, DeviceInfo::GlobalMemSize).unwrap() {
            core::DeviceInfoResult::GlobalMemSize(mem) => {
                total_mem = mem;
                if !quiet {
                    println!(
                        "GPU: {} - {} [RAM={}MiB, Cores={}]",
                        to_string!(core::get_device_info(&device, DeviceInfo::Vendor)),
                        to_string!(core::get_device_info(&device, DeviceInfo::Name)),
                        mem / 1024 / 1024,
                        to_string!(core::get_device_info(&device, DeviceInfo::MaxComputeUnits))
                    );
                }
            }
            _ => panic!("Unexpected error. Can't obtain GPU memory size."),
        }

        // use max 75% of total gpu mem
        // todo: user limit
        let num_buffer = 2;
        let max_nonces = ((total_mem / 8 * 2) / (num_buffer * NONCE_SIZE)) as usize;
        result.push(Arc::new(GpuContext::new(
            platform_id,
            gpu_id,
            max_nonces,
            false,
        )));
    }
    result
}

// Ohne Gummi im Bahnhofsviertel... das wird noch Konsequenzen haben
unsafe impl Sync for GpuContext {}
//unsafe impl Send for GpuBuffer {}

impl GpuContext {
    pub fn new(
        gpu_platform: usize,
        gpu_id: usize,
        max_nonces_per_cache: usize,
        mapping: bool,
    ) -> GpuContext {
        let platform_ids = core::get_platform_ids().unwrap();
        let platform_id = platform_ids[gpu_platform];
        let device_ids = core::get_device_ids(&platform_id, None, None).unwrap();
        let device_id = device_ids[gpu_id];
        let context_properties = ContextProperties::new().platform(platform_id);
        let context =
            core::create_context(Some(&context_properties), &[device_id], None, None).unwrap();
        let src_cstring = CString::new(SRC).unwrap();
        let program = core::create_program_with_source(&context, &[src_cstring]).unwrap();
        core::build_program(
            &program,
            None::<&[()]>,
            &CString::new("").unwrap(),
            None,
            None,
        ).unwrap();
        let queue = core::create_command_queue(&context, &device_id, None).unwrap();
        let kernel1 = core::create_kernel(&program, "calculate_nonces").unwrap();

        let kernel1_workgroup_size = get_kernel_work_group_size(&kernel1, device_id);

        println!("Debug: max_nonces_per_cache={}", max_nonces_per_cache);
        let mut workgroup_count = max_nonces_per_cache / kernel1_workgroup_size;
        //if max_nonces_per_cache % kernel1_workgroup_size != 0 {
        //    workgroup_count += 1;
        //}

        let worksize = kernel1_workgroup_size * workgroup_count;

        println!("Debug: worksize={}", worksize);

        let gdim1 = [worksize, 1, 1];
        let ldim1 = [kernel1_workgroup_size, 1, 1];

        // create buffer
        print!("Creating Buffer...");
        let buffer_gpu_a = unsafe {
            core::create_buffer::<_, u8>(
                &context,
                core::MEM_READ_WRITE,
                (NONCE_SIZE as usize) * worksize,
                None,
            ).unwrap()
        };
        let buffer_gpu_b = unsafe {
            core::create_buffer::<_, u8>(
                &context,
                core::MEM_READ_WRITE,
                (NONCE_SIZE as usize) * worksize,
                None,
            ).unwrap()
        };

        let buffer_host_a = vec![1u8; (NONCE_SIZE as usize) * worksize as usize];

        println!("OK");
        GpuContext {
            context,
            queue,
            kernel1,
            ldim1,
            gdim1,
            mapping,
            buffer_gpu_a,
            buffer_gpu_b,
            buffer_host_a: buffer_host_a,
            worksize,
        }
    }
}
/*
pub struct GpuBuffer {
    data: Arc<Mutex<Vec<u8>>>,
    context: Arc<Mutex<GpuContext>>,
    gensig_gpu: core::Mem,
    data_gpu: core::Mem,
    deadlines_gpu: core::Mem,
    best_deadline_gpu: core::Mem,
    best_offset_gpu: core::Mem,
    memmap: Option<Arc<core::MemMap<u8>>>,
}

impl GpuBuffer {
    pub fn new(context_mu: &Arc<Mutex<GpuContext>>) -> Self
    where
        Self: Sized,
    {
        let context = context_mu.lock().unwrap();

        let gensig_gpu = unsafe {
            core::create_buffer::<_, u8>(&context.context, core::MEM_READ_ONLY, 32, None).unwrap()
        };

        let deadlines_gpu = unsafe {
            core::create_buffer::<_, u64>(
                &context.context,
                core::MEM_READ_WRITE,
                context.gdim1[0],
                None,
            ).unwrap()
        };

        let best_offset_gpu = unsafe {
            core::create_buffer::<_, u64>(&context.context, core::MEM_READ_WRITE, 1, None).unwrap()
        };

        let best_deadline_gpu = unsafe {
            core::create_buffer::<_, u64>(&context.context, core::MEM_READ_WRITE, 1, None).unwrap()
        };

        let pointer = aligned_alloc::aligned_alloc(&context.gdim1[0] * 64, page_size::get());
        let data: Vec<u8>;
        unsafe {
            data = Vec::from_raw_parts(
                pointer as *mut u8,
                &context.gdim1[0] * 64,
                &context.gdim1[0] * 64,
            );
        }

        let data_gpu = unsafe {
            core::create_buffer(
                &context.context,
                core::MEM_READ_ONLY | core::MEM_USE_HOST_PTR,
                context.gdim1[0] * 64,
                Some(&data),
            ).unwrap()
        };

        GpuBuffer {
            data: Arc::new(Mutex::new(data)),
            context: context_mu.clone(),
            gensig_gpu,
            data_gpu,
            deadlines_gpu,
            best_deadline_gpu,
            best_offset_gpu,
            memmap: None,
        }
    }
    /*
}

impl Buffer for GpuBuffer {
    */
    fn get_buffer_for_writing(&mut self) -> Arc<Mutex<Vec<u8>>> {
        // pointer is cached, however, calling enqueue map to make DMA work.
        let locked_context = self.context.lock().unwrap();
        if locked_context.mapping {
            unsafe {
                self.memmap = Some(Arc::new(
                    core::enqueue_map_buffer::<u8, _, _, _>(
                        &(*locked_context).queue,
                        &self.data_gpu,
                        true,
                        core::MAP_WRITE,
                        0,
                        &(*locked_context).gdim1[0] * 64,
                        None::<Event>,
                        None::<&mut Event>,
                    ).unwrap(),
                ));
            }
        }
        self.data.clone()
    }

    fn get_buffer(&mut self) -> Arc<Mutex<Vec<u8>>> {
        self.data.clone()
    }

    fn get_gpu_context(&self) -> Option<Arc<Mutex<GpuContext>>> {
        Some(self.context.clone())
    }
    fn get_gpu_buffers(&self) -> Option<&GpuBuffer> {
        Some(self)
    }
}
*/

pub fn noncegen_gpu(
    cache: *mut u8,
    cache_size: u64,
    chunk_offset: u64,
    numeric_id: u64,
    local_startnonce: u64,
    local_nonces: u64,
    gpu_context: Arc<GpuContext>,
) {
    // to be changed!
/*
    unsafe {
       let data = from_raw_parts(cache,  NONCE_SIZE as usize * cache_size as usize);
    }
*/
    let numeric_id_be: u64 = unsafe { transmute(numeric_id.to_be()) };

    //let gpu_context = GpuContext::new(0, 0, local_nonces as usize, false);

    //let data_gpu = unsafe {
    //    core::create_buffer::<_, u8>(&gpu_context.context, core::MEM_READ_WRITE, (NONCE_SIZE * 1024) as usize, None).unwrap()
    //};

    let hashes_per_run: usize = 32;
    let mut start = 0;
    let mut end = 0;

            core::set_kernel_arg(
            &gpu_context.kernel1,
            0,
            ArgVal::mem(&gpu_context.buffer_gpu_a),
        ).unwrap();
        core::set_kernel_arg(
            &gpu_context.kernel1,
            1,
            ArgVal::primitive(&local_startnonce),
        ).unwrap();
        core::set_kernel_arg(&gpu_context.kernel1, 2, ArgVal::primitive(&numeric_id_be)).unwrap();

    for i in (0..8192).step_by(hashes_per_run) {
        if i + hashes_per_run < 8192 {
            start = i;
            end = i + hashes_per_run - 1;
        } else {
            start = i;
            end = i + hashes_per_run;
        }


        core::set_kernel_arg(&gpu_context.kernel1, 3, ArgVal::primitive(&(start as i32))).unwrap();
        core::set_kernel_arg(&gpu_context.kernel1, 4, ArgVal::primitive(&(end as i32))).unwrap();

        unsafe {
            core::enqueue_kernel(
                &gpu_context.queue,
                &gpu_context.kernel1,
                1,
                None,
                &gpu_context.gdim1,
                Some(gpu_context.ldim1),
                None::<Event>,
                None::<&mut Event>,
            ).unwrap();
        }
        core::finish(&gpu_context.queue);
    }
    return;

    core::set_kernel_arg(
        &gpu_context.kernel1,
        0,
        ArgVal::mem(&gpu_context.buffer_gpu_a),
    ).unwrap();
    core::set_kernel_arg(
        &gpu_context.kernel1,
        1,
        ArgVal::primitive(&local_startnonce),
    ).unwrap();
    core::set_kernel_arg(&gpu_context.kernel1, 2, ArgVal::primitive(&numeric_id_be)).unwrap();

    unsafe {
        core::enqueue_kernel(
            &gpu_context.queue,
            &gpu_context.kernel1,
            1,
            None,
            &gpu_context.gdim1,
            Some(gpu_context.ldim1),
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    }
    core::finish(&gpu_context.queue);

    /*
    let mut datax = vec![1u8;  (NONCE_SIZE * local_nonces) as usize];
    //let mut datax =  &gpu_context.buffer_host_a.lock().unwrap();
    unsafe {
        core::enqueue_read_buffer(
            &gpu_context.queue,
            &gpu_context.buffer_gpu_a,
            true,
            0,
            &mut datax[0..],
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    }
    
*/

    /*   
    core::set_kernel_arg(&gpu_context.kernel1, 0, ArgVal::mem(&buffer.gensig_gpu)).unwrap();
    core::set_kernel_arg(&gpu_context.kernel1, 1, ArgVal::mem(&buffer.data_gpu)).unwrap();
    core::set_kernel_arg(&gpu_context.kernel1, 2, ArgVal::mem(&buffer.deadlines_gpu)).unwrap();

    unsafe {
        core::enqueue_kernel(
            &gpu_context.queue,
            &gpu_context.kernel1,
            1,
            None,
            &gpu_context.gdim1,
            Some(gpu_context.ldim1),
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    }

 return;
    */
    /*    
    let gpu_context_mtx = (*buffer).get_gpu_context().unwrap();
    let gpu_context = gpu_context_mtx.lock().unwrap();

    unsafe {
        core::enqueue_write_buffer(
            &gpu_context.queue,
            &buffer.gensig_gpu,
            false,
            0,
            &gensig,
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    }

    if gpu_context.mapping {
        let temp = buffer.memmap.clone();
        let temp2 = temp.unwrap();
        core::  (
            &gpu_context.queue,
            &buffer.data_gpu,
            &*temp2,
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    } else {
        unsafe {
            core::enqueue_write_buffer(
                &gpu_context.queue,
                &buffer.data_gpu,
                false,
                0,
                &data2,
                None::<Event>,
                None::<&mut Event>,
            ).unwrap();
        }
    }

    core::set_kernel_arg(&gpu_context.kernel1, 0, ArgVal::mem(&buffer.gensig_gpu)).unwrap();
    core::set_kernel_arg(&gpu_context.kernel1, 1, ArgVal::mem(&buffer.data_gpu)).unwrap();
    core::set_kernel_arg(&gpu_context.kernel1, 2, ArgVal::mem(&buffer.deadlines_gpu)).unwrap();

    unsafe {
        core::enqueue_kernel(
            &gpu_context.queue,
            &gpu_context.kernel1,
            1,
            None,
            &gpu_context.gdim1,
            Some(gpu_context.ldim1),
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    }

    core::set_kernel_arg(&gpu_context.kernel2, 0, ArgVal::mem(&buffer.deadlines_gpu)).unwrap();
    core::set_kernel_arg(&gpu_context.kernel2, 1, ArgVal::primitive(&nonce_count)).unwrap();
    core::set_kernel_arg(
        &gpu_context.kernel2,
        2,
        ArgVal::local::<u32>(&gpu_context.ldim2[0]),
    ).unwrap();
    core::set_kernel_arg(
        &gpu_context.kernel2,
        3,
        ArgVal::mem(&buffer.best_offset_gpu),
    ).unwrap();
    core::set_kernel_arg(
        &gpu_context.kernel2,
        4,
        ArgVal::mem(&buffer.best_deadline_gpu),
    ).unwrap();

    unsafe {
        core::enqueue_kernel(
            &gpu_context.queue,
            &gpu_context.kernel2,
            1,
            None,
            &gpu_context.gdim2,
            Some(gpu_context.ldim2),
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    }

    let mut best_offset = vec![0u64; 1];
    let mut best_deadline = vec![0u64; 1];

    unsafe {
        core::enqueue_read_buffer(
            &gpu_context.queue,
            &buffer.best_offset_gpu,
            true,
            0,
            &mut best_offset,
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    }
    unsafe {
        core::enqueue_read_buffer(
            &gpu_context.queue,
            &buffer.best_deadline_gpu,
            true,
            0,
            &mut best_deadline,
            None::<Event>,
            None::<&mut Event>,
        ).unwrap();
    }
    */
}

fn get_kernel_work_group_size(x: &core::Kernel, y: core::DeviceId) -> usize {
    match core::get_kernel_work_group_info(x, y, KernelWorkGroupInfo::WorkGroupSize).unwrap() {
        core::KernelWorkGroupInfoResult::WorkGroupSize(kws) => kws,
        _ => panic!("Unexpected error"),
    }
}