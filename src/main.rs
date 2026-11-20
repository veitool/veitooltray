#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// 调试时默认保留控制台；如需调试版也完全静默，可改成：
// #![windows_subsystem = "windows"]

#[cfg(windows)]
mod app {
    use std::os::windows::process::CommandExt;
    use std::{
        error::Error,
        ffi::c_void,
        process::Command,
        sync::{
            Mutex, OnceLock,
            atomic::{AtomicBool, AtomicPtr, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use sysinfo::{CpuRefreshKind, MemoryRefreshKind, Networks, RefreshKind, System};
    use windows_sys::Win32::{
        Foundation::{GetLastError, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, CLIP_DEFAULT_PRECIS, COLOR_INFOBK, COLOR_INFOTEXT, COLOR_WINDOWTEXT,
            CreateFontW, CreateSolidBrush, DEFAULT_CHARSET, DEFAULT_PITCH, DT_LEFT, DT_NOPREFIX,
            DT_SINGLELINE, DT_TOP, DT_VCENTER, DeleteObject, DrawTextW, EndPaint, FF_DONTCARE,
            FW_NORMAL, FillRect, GetSysColor, GetSysColorBrush, InvalidateRect,
            NONANTIALIASED_QUALITY, OUT_DEFAULT_PRECIS, ScreenToClient, SelectObject, SetBkMode,
            SetTextColor, TRANSPARENT, UpdateWindow,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::Controls::{HOVER_DEFAULT, WM_MOUSEHOVER, WM_MOUSELEAVE},
        UI::Input::KeyboardAndMouse::{TME_HOVER, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent},
        UI::WindowsAndMessaging::{
            AppendMenuW, CS_HREDRAW, CS_VREDRAW, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
            DestroyMenu, FindWindowExW, FindWindowW, GetClientRect, GetCursorPos, GetSystemMetrics,
            GetWindowRect, GetWindowTextW, HTCLIENT, IsWindow, LWA_ALPHA, LWA_COLORKEY, MF_STRING,
            RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
            SetLayeredWindowAttributes, SetWindowPos, SetWindowTextW, ShowWindow, TPM_RETURNCMD,
            TPM_RIGHTBUTTON, TrackPopupMenu, WM_ERASEBKGND, WM_MOUSEMOVE, WM_NCHITTEST, WM_PAINT,
            WM_RBUTTONUP, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
        },
    };
    use winit::{
        application::ApplicationHandler,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
        window::WindowId,
    };

    // 修改此处配置刷新频率；sysinfo 的 CPU 采样不适合低于约 200ms。
    const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
    // None 汇总所有网卡；改为 Some("Wi-Fi") 等名称可只统计指定网卡。
    const NETWORK_NAME_FILTER: Option<&str> = None;
    // 使用 1024 进制显示 KB/MB；改为 1000.0 可使用十进制单位。
    const BYTES_PER_KB: f64 = 1024.0;
    const TEMPERATURE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const TASKBAR_TEXT_WIDTH: i32 = 320;
    const TASKBAR_TEXT_HEIGHT: i32 = 55;
    const TASKBAR_COLUMN_SPLIT: i32 = 180;
    const TASKBAR_ROW_GAP: i32 = 5;
    const TEMPERATURE_TOOLTIP_WIDTH: i32 = 410;
    const TEMPERATURE_TOOLTIP_HEIGHT: i32 = 76;
    static EVENT_PROXY: OnceLock<EventLoopProxy<UserEvent>> = OnceLock::new();
    static TASKBAR_FONT: OnceLock<usize> = OnceLock::new();
    static TOOLTIP_FONT: OnceLock<usize> = OnceLock::new();
    static TEMPERATURE_TOOLTIP_TEXT: OnceLock<Mutex<String>> = OnceLock::new();
    static TEMPERATURE_TOOLTIP_HWND: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static MOUSE_TRACKING: AtomicBool = AtomicBool::new(false);

    #[derive(Debug)]
    enum UserEvent {
        Metrics { text: String },
        Temperature(String),
        Exit,
    }

    struct App {
        taskbar_text: TaskbarText,
        failure: Option<String>,
    }

    impl ApplicationHandler<UserEvent> for App {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            event_loop.set_control_flow(ControlFlow::Wait);
        }

        fn window_event(
            &mut self,
            _event_loop: &ActiveEventLoop,
            _window_id: WindowId,
            _event: WindowEvent,
        ) {
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            match event {
                UserEvent::Metrics { text } => {
                    if let Err(error) = self.taskbar_text.update(&text) {
                        self.failure = Some(format!("更新任务栏状态失败：{error}"));
                        event_loop.exit();
                    }
                }
                UserEvent::Temperature(text) => {
                    if let Err(error) = set_temperature_tooltip_text(&text) {
                        self.failure = Some(format!("更新温度提示失败：{error}"));
                        event_loop.exit();
                    }
                }
                UserEvent::Exit => event_loop.exit(),
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
        let proxy = event_loop.create_proxy();
        EVENT_PROXY
            .set(proxy.clone())
            .map_err(|_| "taskbar event proxy was already initialized")?;
        create_taskbar_font()?;
        create_tooltip_font()?;
        set_temperature_tooltip_text("CPU 温度：不可用（未检测到传感器）\nGPU 温度：读取中")?;

        start_sampler(proxy)?;

        let mut app = App {
            taskbar_text: TaskbarText::new(),
            failure: None,
        };
        event_loop.run_app(&mut app)?;

        if let Some(error) = app.failure {
            return Err(error.into());
        }
        Ok(())
    }

    fn start_sampler(proxy: EventLoopProxy<UserEvent>) -> Result<(), Box<dyn Error>> {
        let temperature_proxy = proxy.clone();
        thread::Builder::new()
            .name("veitooltray-sampler".to_owned())
            .spawn(move || {
                let mut system = System::new_with_specifics(
                    RefreshKind::nothing()
                        .with_cpu(CpuRefreshKind::everything())
                        .with_memory(MemoryRefreshKind::nothing().with_ram()),
                );
                let mut networks = Networks::new_with_refreshed_list();

                // CPU 使用率和网速均通过相邻两次采样的差值计算。
                system.refresh_cpu_usage();
                let mut previous_sample = Instant::now();

                loop {
                    thread::sleep(REFRESH_INTERVAL);
                    system.refresh_cpu_usage();
                    system.refresh_memory();
                    networks.refresh(true);
                    let now = Instant::now();
                    let elapsed = now.duration_since(previous_sample).as_secs_f64();
                    previous_sample = now;

                    let (received, transmitted) = network_deltas(&networks);
                    let total_memory = system.total_memory();
                    let memory_percent = if total_memory == 0 {
                        0.0
                    } else {
                        system.used_memory() as f64 * 100.0 / total_memory as f64
                    };
                    let tooltip = format!(
                        "↑: {}\tCPU: {:.0} %\n↓: {}\t内存: {:.0} %",
                        format_speed(transmitted as f64 / elapsed),
                        system.global_cpu_usage(),
                        format_speed(received as f64 / elapsed),
                        memory_percent,
                    );

                    if proxy
                        .send_event(UserEvent::Metrics { text: tooltip })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;
        thread::Builder::new()
            .name("temperature-sampler".to_owned())
            .spawn(move || {
                loop {
                    if temperature_proxy
                        .send_event(UserEvent::Temperature(temperature_tooltip_text()))
                        .is_err()
                    {
                        break;
                    }
                    thread::sleep(TEMPERATURE_REFRESH_INTERVAL);
                }
            })?;
        Ok(())
    }

    fn network_deltas(networks: &Networks) -> (u64, u64) {
        networks
            .iter()
            .filter(|(name, _)| NETWORK_NAME_FILTER.is_none_or(|filter| name.as_str() == filter))
            .fold((0_u64, 0_u64), |(received, transmitted), (_, network)| {
                (
                    received.saturating_add(network.received()),
                    transmitted.saturating_add(network.transmitted()),
                )
            })
    }

    fn format_speed(bytes_per_second: f64) -> String {
        let kb_per_second = bytes_per_second / BYTES_PER_KB;
        if kb_per_second >= BYTES_PER_KB {
            format!("{:.1} MB/s", kb_per_second / BYTES_PER_KB)
        } else {
            format!("{kb_per_second:.1} KB/s")
        }
    }

    fn temperature_tooltip_text() -> String {
        let gpu_temperature = read_nvidia_gpu_temperature()
            .map(|temperature| format!("{temperature} °C"))
            .unwrap_or_else(|| "不可用".to_owned());
        format!("CPU 温度：不可用（未检测到传感器）\nGPU 温度：{gpu_temperature}")
    }

    fn read_nvidia_gpu_temperature() -> Option<u32> {
        let output = Command::new("nvidia-smi")
            .args([
                "--query-gpu=temperature.gpu",
                "--format=csv,noheader,nounits",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }

    // Keep an owned popup aligned with Explorer's taskbar; never create a cross-process child.
    struct TaskbarText {
        hwnd: Option<HWND>,
        click_target: Option<HWND>,
        temperature_tooltip: Option<HWND>,
        owner: Option<HWND>,
    }

    impl TaskbarText {
        fn new() -> Self {
            Self {
                hwnd: None,
                click_target: None,
                temperature_tooltip: None,
                owner: None,
            }
        }

        fn update(&mut self, text: &str) -> Result<(), Box<dyn Error>> {
            let taskbar_class = wide("Shell_TrayWnd");
            let notify_class = wide("TrayNotifyWnd");
            let taskbar = unsafe { FindWindowW(taskbar_class.as_ptr(), std::ptr::null()) };
            if taskbar.is_null() {
                if let Some(hwnd) = self.hwnd {
                    unsafe { ShowWindow(hwnd, SW_HIDE) };
                }
                if let Some(hwnd) = self.click_target {
                    unsafe { ShowWindow(hwnd, SW_HIDE) };
                }
                return Ok(());
            }

            if self.owner != Some(taskbar)
                || self.hwnd.is_none_or(|hwnd| unsafe { IsWindow(hwnd) == 0 })
                || self
                    .click_target
                    .is_none_or(|hwnd| unsafe { IsWindow(hwnd) == 0 })
            {
                self.destroy_window();
                let text_window = create_taskbar_window(taskbar, false)?;
                let click_target = match create_taskbar_window(taskbar, true) {
                    Ok(hwnd) => hwnd,
                    Err(error) => {
                        unsafe {
                            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(text_window)
                        };
                        return Err(error);
                    }
                };
                let temperature_tooltip = match create_temperature_tooltip_window(text_window) {
                    Ok(hwnd) => hwnd,
                    Err(error) => {
                        unsafe {
                            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(
                                click_target,
                            );
                            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(text_window);
                        }
                        return Err(error);
                    }
                };
                self.hwnd = Some(text_window);
                self.click_target = Some(click_target);
                self.temperature_tooltip = Some(temperature_tooltip);
                self.owner = Some(taskbar);
            }

            let hwnd = self.hwnd.expect("taskbar child was created above");
            let click_target = self
                .click_target
                .expect("taskbar click target was created above");
            let mut taskbar_rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let mut notify_rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            let notify_area = unsafe {
                FindWindowExW(
                    taskbar,
                    std::ptr::null_mut(),
                    notify_class.as_ptr(),
                    std::ptr::null(),
                )
            };

            if unsafe { GetWindowRect(taskbar, &mut taskbar_rect) } == 0 {
                return Err(last_windows_error().into());
            }

            let has_notify_rect = !notify_area.is_null()
                && unsafe { GetWindowRect(notify_area, &mut notify_rect) } != 0;
            let taskbar_width = taskbar_rect.right - taskbar_rect.left;
            let taskbar_height = taskbar_rect.bottom - taskbar_rect.top;
            let is_vertical = taskbar_height > taskbar_width;
            let (x, y) = if is_vertical && has_notify_rect {
                let mut point = windows_sys::Win32::Foundation::POINT {
                    x: notify_rect.left,
                    y: notify_rect.top,
                };
                if unsafe { ScreenToClient(taskbar, &mut point) } == 0 {
                    return Err(last_windows_error().into());
                }
                (
                    taskbar_rect.left + (taskbar_width - TASKBAR_TEXT_WIDTH) / 2,
                    taskbar_rect.top + point.y - TASKBAR_TEXT_HEIGHT - 8,
                )
            } else if has_notify_rect {
                (
                    notify_rect.left - TASKBAR_TEXT_WIDTH - 8,
                    taskbar_rect.top + (taskbar_height - TASKBAR_TEXT_HEIGHT) / 2,
                )
            } else {
                (
                    taskbar_rect.right - TASKBAR_TEXT_WIDTH,
                    taskbar_rect.top + (taskbar_height - TASKBAR_TEXT_HEIGHT) / 2,
                )
            };

            let text = wide(text);
            if unsafe { SetWindowTextW(hwnd, text.as_ptr()) } == 0 {
                return Err(last_windows_error().into());
            }
            if unsafe {
                SetWindowPos(
                    hwnd,
                    taskbar,
                    x,
                    y,
                    TASKBAR_TEXT_WIDTH,
                    TASKBAR_TEXT_HEIGHT,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
            } == 0
            {
                return Err(last_windows_error().into());
            }
            if unsafe {
                SetWindowPos(
                    click_target,
                    hwnd,
                    x,
                    y,
                    TASKBAR_TEXT_WIDTH,
                    TASKBAR_TEXT_HEIGHT,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
            } == 0
            {
                return Err(last_windows_error().into());
            }
            unsafe {
                InvalidateRect(hwnd, std::ptr::null(), 1);
                UpdateWindow(hwnd);
            }
            Ok(())
        }

        fn destroy_window(&mut self) {
            if let Some(hwnd) = self.temperature_tooltip.take()
                && unsafe { IsWindow(hwnd) } != 0
            {
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
            }
            TEMPERATURE_TOOLTIP_HWND.store(std::ptr::null_mut(), Ordering::Relaxed);
            MOUSE_TRACKING.store(false, Ordering::Relaxed);
            if let Some(hwnd) = self.hwnd.take()
                && unsafe { IsWindow(hwnd) } != 0
            {
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
            }
            if let Some(hwnd) = self.click_target.take()
                && unsafe { IsWindow(hwnd) } != 0
            {
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd) };
            }
            self.owner = None;
        }
    }

    fn create_temperature_tooltip_window(owner: HWND) -> Result<HWND, Box<dyn Error>> {
        let class_name = wide("VeitoolTrayTemperatureTooltip");
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        if module.is_null() {
            return Err(last_windows_error().into());
        }

        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(temperature_tooltip_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            let error = unsafe { GetLastError() };
            const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
            if error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(std::io::Error::from_raw_os_error(error as i32).into());
            }
        }

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
                class_name.as_ptr(),
                wide("").as_ptr(),
                WS_POPUP,
                0,
                0,
                TEMPERATURE_TOOLTIP_WIDTH,
                TEMPERATURE_TOOLTIP_HEIGHT,
                owner,
                std::ptr::null_mut(),
                module,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(last_windows_error().into());
        }
        TEMPERATURE_TOOLTIP_HWND.store(hwnd, Ordering::Relaxed);
        Ok(hwnd)
    }

    fn create_tooltip_font() -> Result<(), Box<dyn Error>> {
        let face_name = wide("Microsoft YaHei");
        let font = unsafe {
            CreateFontW(
                -22,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                NONANTIALIASED_QUALITY as u32,
                (DEFAULT_PITCH | FF_DONTCARE) as u32,
                face_name.as_ptr(),
            )
        };
        if font.is_null() {
            return Err(last_windows_error().into());
        }
        TOOLTIP_FONT
            .set(font as usize)
            .map_err(|_| "temperature tooltip font was already initialized")?;
        Ok(())
    }

    impl Drop for TaskbarText {
        fn drop(&mut self) {
            self.destroy_window();
        }
    }

    fn create_taskbar_window(owner: HWND, click_target: bool) -> Result<HWND, Box<dyn Error>> {
        let class_name = wide("VeitoolTrayStatusTaskbarText");
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        if module.is_null() {
            return Err(last_windows_error().into());
        }

        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(taskbar_text_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            let error = unsafe { GetLastError() };
            const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
            if error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(std::io::Error::from_raw_os_error(error as i32).into());
            }
        }

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                wide("").as_ptr(),
                WS_POPUP | WS_VISIBLE,
                0,
                0,
                TASKBAR_TEXT_WIDTH,
                TASKBAR_TEXT_HEIGHT,
                owner,
                std::ptr::null_mut(),
                module,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(last_windows_error().into());
        }
        let layered_style = if click_target {
            unsafe { SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA) }
        } else {
            const TRANSPARENT_COLOR: u32 = 0x00FF_00FF;
            unsafe { SetLayeredWindowAttributes(hwnd, TRANSPARENT_COLOR, 0, LWA_COLORKEY) }
        };
        if layered_style == 0 {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            }
            return Err(last_windows_error().into());
        }
        unsafe {
            SetWindowPos(
                hwnd,
                owner,
                0,
                0,
                TASKBAR_TEXT_WIDTH,
                TASKBAR_TEXT_HEIGHT,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        Ok(hwnd)
    }

    fn create_taskbar_font() -> Result<(), Box<dyn Error>> {
        let face_name = wide("Microsoft YaHei");
        let font = unsafe {
            CreateFontW(
                -24,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                NONANTIALIASED_QUALITY as u32,
                (DEFAULT_PITCH | FF_DONTCARE) as u32,
                face_name.as_ptr(),
            )
        };
        if font.is_null() {
            return Err(last_windows_error().into());
        }
        TASKBAR_FONT
            .set(font as usize)
            .map_err(|_| "taskbar font was already initialized")?;
        Ok(())
    }

    unsafe extern "system" fn taskbar_text_wnd_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_ERASEBKGND => 1,
            WM_NCHITTEST => HTCLIENT as LRESULT,
            WM_MOUSEMOVE => {
                if !MOUSE_TRACKING.swap(true, Ordering::Relaxed) {
                    let mut tracking = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_HOVER | TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: HOVER_DEFAULT,
                    };
                    if unsafe { TrackMouseEvent(&mut tracking) } == 0 {
                        MOUSE_TRACKING.store(false, Ordering::Relaxed);
                    }
                }
                0
            }
            WM_MOUSEHOVER => {
                show_temperature_tooltip();
                0
            }
            WM_MOUSELEAVE => {
                MOUSE_TRACKING.store(false, Ordering::Relaxed);
                let tooltip = TEMPERATURE_TOOLTIP_HWND.load(Ordering::Relaxed);
                if !tooltip.is_null() {
                    unsafe { ShowWindow(tooltip, SW_HIDE) };
                }
                0
            }
            WM_RBUTTONUP => {
                show_exit_menu(hwnd);
                0
            }
            WM_PAINT => {
                let mut paint = std::mem::MaybeUninit::zeroed();
                let hdc = unsafe { BeginPaint(hwnd, paint.as_mut_ptr()) };
                let mut client = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                unsafe { GetClientRect(hwnd, &mut client) };
                let background = unsafe { CreateSolidBrush(0x00FF_00FF) };
                if !background.is_null() {
                    unsafe {
                        FillRect(hdc, &client, background);
                        DeleteObject(background);
                    }
                }
                let font = *TASKBAR_FONT
                    .get()
                    .expect("taskbar font is initialized before the event loop")
                    as _;
                let previous_font = unsafe { SelectObject(hdc, font) };
                unsafe {
                    SetBkMode(hdc, TRANSPARENT as i32);
                    SetTextColor(hdc, GetSysColor(COLOR_WINDOWTEXT));
                }

                let mut buffer = [0_u16; 256];
                let length =
                    unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
                if length > 0 {
                    let text = String::from_utf16_lossy(&buffer[..length as usize]);
                    let row_height = (client.bottom - TASKBAR_ROW_GAP) / 2;
                    for (row, line) in text.lines().take(2).enumerate() {
                        let Some((left_text, right_text)) = line.split_once('\t') else {
                            continue;
                        };

                        let top = row as i32 * (row_height + TASKBAR_ROW_GAP);
                        let mut left_rect = RECT {
                            left: 6,
                            top,
                            right: TASKBAR_COLUMN_SPLIT - 8,
                            bottom: top + row_height,
                        };
                        let mut right_rect = RECT {
                            left: TASKBAR_COLUMN_SPLIT,
                            top,
                            right: client.right - 4,
                            bottom: top + row_height,
                        };
                        let left_buffer = wide(left_text);
                        let right_buffer = wide(right_text);
                        unsafe {
                            DrawTextW(
                                hdc,
                                left_buffer.as_ptr(),
                                left_buffer.len() as i32 - 1,
                                &mut left_rect,
                                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                            );
                            DrawTextW(
                                hdc,
                                right_buffer.as_ptr(),
                                right_buffer.len() as i32 - 1,
                                &mut right_rect,
                                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                            );
                        }
                    }
                }
                unsafe {
                    SelectObject(hdc, previous_font);
                    EndPaint(hwnd, paint.as_mut_ptr());
                }
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    fn set_temperature_tooltip_text(text: &str) -> Result<(), Box<dyn Error>> {
        let text_store = TEMPERATURE_TOOLTIP_TEXT.get_or_init(|| Mutex::new(String::new()));
        *text_store
            .lock()
            .map_err(|_| "temperature tooltip text lock is poisoned")? = text.to_owned();
        let hwnd = TEMPERATURE_TOOLTIP_HWND.load(Ordering::Relaxed);
        if !hwnd.is_null() {
            unsafe {
                InvalidateRect(hwnd, std::ptr::null(), 1);
                UpdateWindow(hwnd);
            }
        }
        Ok(())
    }

    fn show_temperature_tooltip() {
        let hwnd = TEMPERATURE_TOOLTIP_HWND.load(Ordering::Relaxed);
        if hwnd.is_null() {
            return;
        }
        let mut cursor = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut cursor) } == 0 {
            return;
        }

        let screen_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let screen_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        let mut x = cursor.x + 16;
        let mut y = cursor.y - TEMPERATURE_TOOLTIP_HEIGHT - 16;
        if x + TEMPERATURE_TOOLTIP_WIDTH > screen_width {
            x = screen_width - TEMPERATURE_TOOLTIP_WIDTH - 8;
        }
        if y < 0 {
            y = cursor.y + 24;
        } else if y + TEMPERATURE_TOOLTIP_HEIGHT > screen_height {
            y = screen_height - TEMPERATURE_TOOLTIP_HEIGHT - 8;
        }
        unsafe {
            InvalidateRect(hwnd, std::ptr::null(), 1);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                TEMPERATURE_TOOLTIP_WIDTH,
                TEMPERATURE_TOOLTIP_HEIGHT,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            UpdateWindow(hwnd);
        }
    }

    unsafe extern "system" fn temperature_tooltip_wnd_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut paint = std::mem::MaybeUninit::zeroed();
                let hdc = unsafe { BeginPaint(hwnd, paint.as_mut_ptr()) };
                let mut client = RECT {
                    left: 0,
                    top: 0,
                    right: TEMPERATURE_TOOLTIP_WIDTH,
                    bottom: TEMPERATURE_TOOLTIP_HEIGHT,
                };
                unsafe {
                    FillRect(hdc, &client, GetSysColorBrush(COLOR_INFOBK));
                    SetBkMode(hdc, TRANSPARENT as i32);
                    SetTextColor(hdc, GetSysColor(COLOR_INFOTEXT));
                }
                let font = *TOOLTIP_FONT
                    .get()
                    .expect("temperature tooltip font is initialized before the event loop")
                    as _;
                let previous_font = unsafe { SelectObject(hdc, font) };
                client.left = 8;
                client.top = 6;
                client.right -= 8;
                client.bottom -= 6;
                let text_store = TEMPERATURE_TOOLTIP_TEXT.get_or_init(|| Mutex::new(String::new()));
                let text = text_store
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let buffer = wide(&text);
                unsafe {
                    DrawTextW(
                        hdc,
                        buffer.as_ptr(),
                        buffer.len() as i32 - 1,
                        &mut client,
                        DT_LEFT | DT_TOP | DT_NOPREFIX,
                    );
                    SelectObject(hdc, previous_font);
                    EndPaint(hwnd, paint.as_mut_ptr());
                }
                0
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    fn show_exit_menu(hwnd: HWND) {
        const EXIT_MENU_ID: usize = 1;
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return;
        }

        let exit_label = wide("退出程序");
        if unsafe { AppendMenuW(menu, MF_STRING, EXIT_MENU_ID, exit_label.as_ptr()) } == 0 {
            unsafe { DestroyMenu(menu) };
            return;
        }

        let mut cursor = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut cursor) } != 0 {
            let selected = unsafe {
                TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                    cursor.x,
                    cursor.y,
                    0,
                    hwnd,
                    std::ptr::null(),
                )
            };
            if selected as usize == EXIT_MENU_ID
                && let Some(proxy) = EVENT_PROXY.get()
            {
                let _ = proxy.send_event(UserEvent::Exit);
            }
        }

        unsafe { DestroyMenu(menu) };
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn last_windows_error() -> std::io::Error {
        std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
    }
}

#[cfg(windows)]
fn main() {
    if let Err(error) = app::run() {
        eprintln!("{error}");
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("VeitoolTray is a Windows system-tray application.");
}
