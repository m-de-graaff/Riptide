//! Showing a title's frames: a window and a swap chain.
//!
//! A title presents a texture of its own. The window's swap chain has buffers of that
//! texture's size and format, the texture is copied into the next buffer, and Windows
//! scales the buffer to the window. Frames are shown as soon as they are ready, with
//! tearing allowed, so that a variable-refresh screen follows the title (decision S2).
//!
//! The window covers the screen without a border. Setting `RIPTIDE_WINDOW=1280x720` makes
//! it an ordinary window of that size instead. Closing the window ends the title.
//!
//! Not built: high dynamic range (S3), more than one plane, source and destination
//! rectangles, and following the launcher's display settings.

use std::sync::Mutex;
use std::sync::mpsc;

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_FENCE_FLAG_NONE, D3D12_RESOURCE_BARRIER,
    D3D12_RESOURCE_BARRIER_0, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
    D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
    D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE,
    D3D12_RESOURCE_STATES, D3D12_RESOURCE_TRANSITION_BARRIER, ID3D12CommandAllocator,
    ID3D12CommandList, ID3D12CommandQueue, ID3D12Fence, ID3D12GraphicsCommandList, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT_ALLOW_TEARING,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIFactory4, IDXGISwapChain3,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW,
    GetMessageW, GetSystemMetrics, IDC_ARROW, LoadCursorW, MSG, PostQuitMessage, RegisterClassW,
    SM_CXSCREEN, SM_CYSCREEN, SW_SHOW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE,
    WM_DESTROY, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_POPUP,
};
use windows::core::{Interface, w};

use crate::trace;

/// The size of an ordinary window asked for with `RIPTIDE_WINDOW=WxH`, if one was.
fn asked_size() -> Option<(i32, i32)> {
    parse_size(&std::env::var("RIPTIDE_WINDOW").ok()?)
}

/// `1280x720` as a width and a height, if both are sizes a window can have.
fn parse_size(text: &str) -> Option<(i32, i32)> {
    let (width, height) = text.trim().split_once(['x', 'X'])?;
    let (width, height) = (width.trim().parse().ok()?, height.trim().parse().ok()?);
    ((160..=16384).contains(&width) && (90..=16384).contains(&height)).then_some((width, height))
}

/// The window's procedure: closing the window ends the title.
unsafe extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE | WM_DESTROY => {
            // SAFETY: a plain call.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: the default procedure, with the message as received.
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

/// Makes the window on a thread of its own, which then answers the window's messages for
/// as long as the process lives. Returns the window's handle as a number.
fn make_window() -> Option<isize> {
    let (made, window) = mpsc::channel::<Option<isize>>();
    let thread = std::thread::Builder::new().name("riptide window".to_owned());
    let spawned = thread.spawn(move || {
        // SAFETY: plain window calls on the thread that owns the window; every pointer
        // passed is valid for its call.
        unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_procedure),
                hInstance: instance.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                lpszClassName: w!("RiptideTitle"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let (style, x, y, width, height) = match asked_size() {
                Some((width, height)) => (
                    WS_OVERLAPPEDWINDOW,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    width,
                    height,
                ),
                None => (
                    WS_POPUP,
                    0,
                    0,
                    GetSystemMetrics(SM_CXSCREEN),
                    GetSystemMetrics(SM_CYSCREEN),
                ),
            };
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("RiptideTitle"),
                w!("Riptide"),
                style,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(instance.into()),
                None,
            );
            let Ok(window) = window else {
                let _ = made.send(None);
                return;
            };
            let _ = ShowWindow(window, SW_SHOW);
            let _ = made.send(Some(window.0 as isize));
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        trace!("the window was closed: the title ends");
        std::process::exit(0);
    });
    spawned.ok()?;
    window.recv().ok().flatten()
}

/// The swap chain's format for a texture format: the same, without the sRGB marking,
/// which a flip-model swap chain does not take. `None` for formats it cannot show.
fn shown_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    match format.0 {
        28 | 29 => Some(DXGI_FORMAT(28)),
        87 | 91 => Some(DXGI_FORMAT(87)),
        24 => Some(DXGI_FORMAT(24)),
        10 => Some(DXGI_FORMAT(10)),
        _ => None,
    }
}

/// What shows frames: the window's swap chain, and what copies a frame into it.
struct Presenter {
    queue: ID3D12CommandQueue,
    chain: IDXGISwapChain3,
    size: (u64, u32),
    format: DXGI_FORMAT,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: HANDLE,
    copies: u64,
}

// SAFETY: the interfaces are free-threaded, and the event is a process-wide handle.
unsafe impl Send for Presenter {}

static PRESENTER: Mutex<Option<Presenter>> = Mutex::new(None);

impl Presenter {
    /// A presenter for frames of `size` and `format`, submitted on `queue`.
    fn new(queue: &ID3D12CommandQueue, size: (u64, u32), format: DXGI_FORMAT) -> Option<Presenter> {
        let host = crate::host::host().ok()?;
        let window = HWND(make_window()? as *mut _);
        let description = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.0 as u32,
            Height: size.1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 3,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32,
            ..Default::default()
        };
        // SAFETY: plain calls on the host's interfaces with valid descriptions.
        let made = unsafe {
            (|| -> windows::core::Result<Presenter> {
                let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
                let chain: IDXGISwapChain3 = factory
                    .CreateSwapChainForHwnd(queue, window, &description, None, None)?
                    .cast()?;
                let allocator: ID3D12CommandAllocator = host
                    .device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
                let list: ID3D12GraphicsCommandList = host.device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &allocator,
                    None,
                )?;
                list.Close()?;
                Ok(Presenter {
                    queue: queue.clone(),
                    chain,
                    size,
                    format,
                    allocator,
                    list,
                    fence: host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
                    event: CreateEventW(None, false, false, None)?,
                    copies: 0,
                })
            })()
        };
        match made {
            Ok(presenter) => {
                trace!(
                    "window and swap chain made: frames of {} x {}, format {}",
                    size.0, size.1, format.0
                );
                Some(presenter)
            }
            Err(error) => {
                trace!("no swap chain for the title's frames: {error}");
                None
            }
        }
    }

    /// Copies `frame` into the next buffer and shows it.
    fn show(&mut self, frame: &ID3D12Resource) -> windows::core::Result<()> {
        // SAFETY: plain calls on the host's interfaces. The copy list is reused only when
        // the GPU has finished the copy before.
        unsafe {
            if self.fence.GetCompletedValue() < self.copies {
                self.fence.SetEventOnCompletion(self.copies, self.event)?;
                WaitForSingleObject(self.event, INFINITE);
            }
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            let buffer: ID3D12Resource = self
                .chain
                .GetBuffer(self.chain.GetCurrentBackBufferIndex())?;
            let moves = |from: D3D12_RESOURCE_STATES, to: D3D12_RESOURCE_STATES| {
                [
                    transition(frame, crate::target::REST, from),
                    transition(&buffer, D3D12_RESOURCE_STATE_COMMON, to),
                ]
            };
            self.list.ResourceBarrier(&moves(
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            ));
            self.list.CopyResource(&buffer, frame);
            self.list.ResourceBarrier(&[
                transition(frame, D3D12_RESOURCE_STATE_COPY_SOURCE, crate::target::REST),
                transition(
                    &buffer,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COMMON,
                ),
            ]);
            self.list.Close()?;
            let lists = [Some(self.list.cast::<ID3D12CommandList>()?)];
            self.queue.ExecuteCommandLists(&lists);
            self.copies += 1;
            self.queue.Signal(&self.fence, self.copies)?;
            self.chain.Present(0, DXGI_PRESENT_ALLOW_TEARING).ok()
        }
    }
}

/// A change of state of all of a texture, for one call.
fn transition(
    texture: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a copy of the pointer without a reference of its own. The barrier
                // is used for one call, during which `texture` is alive, and never dropped.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// Shows a title's frame, which the title presented on `queue`. The window and the swap
/// chain are made for the first frame. Returns whether the frame was shown.
pub(crate) fn show(queue: &ID3D12CommandQueue, frame: &ID3D12Resource) -> bool {
    // SAFETY: a plain call on the host texture.
    let description = unsafe { frame.GetDesc() };
    let Some(format) = shown_format(description.Format) else {
        trace!(
            "a frame of format {} cannot be shown by the swap chain",
            description.Format.0
        );
        return false;
    };
    let size = (description.Width, description.Height);
    let mut presenter = PRESENTER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fits = presenter
        .as_ref()
        .is_some_and(|presenter| presenter.size == size && presenter.format == format);
    if !fits {
        if presenter.is_some() {
            trace!("a frame of another size or format than the window's is not shown");
            return false;
        }
        *presenter = Presenter::new(queue, size, format);
    }
    let Some(presenter) = presenter.as_mut() else {
        return false;
    };
    match presenter.show(frame) {
        Ok(()) => true,
        Err(error) => {
            trace!("a frame was not shown: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_size_is_two_sizes_a_window_can_have() {
        assert_eq!(parse_size("1280x720"), Some((1280, 720)));
        assert_eq!(parse_size(" 1920 X 1080 "), Some((1920, 1080)));
        assert_eq!(parse_size("1280"), None);
        assert_eq!(parse_size("0x720"), None);
        assert_eq!(parse_size("wide x tall"), None);
    }

    #[test]
    fn frames_are_shown_without_their_srgb_marking() {
        assert_eq!(shown_format(DXGI_FORMAT(29)), Some(DXGI_FORMAT(28)));
        assert_eq!(shown_format(DXGI_FORMAT(28)), Some(DXGI_FORMAT(28)));
        assert_eq!(shown_format(DXGI_FORMAT(91)), Some(DXGI_FORMAT(87)));
        assert_eq!(shown_format(DXGI_FORMAT(71)), None);
    }
}
