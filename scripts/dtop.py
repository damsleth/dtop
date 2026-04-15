#!/usr/bin/env python3
"""
dtop - A minimal macOS Activity Monitor clone for the terminal
Sums CPU and Memory usage by process name, just like Activity Monitor
"""

import curses
import subprocess
import time
import signal
import sys
import os
from collections import defaultdict
from collections import deque
import statistics


class ProcessMonitor:
    def __init__(self):
        self.poll_interval = 1000  # milliseconds
        self.sort_by = 'cpu'  # 'cpu', 'mem', or 'name'
        self.lazy_mode = True
        self.lazy_window = 10
        self.sort_descending = True
        self.running = True
        self.last_update = 0
        self.show_help = False
        self.current_processes = []
        self.filtered_processes = []
        self.selected_index = None
        self.show_extended = False
        self.search_mode = False
        self.search_query = ''
        self.detail_info = []
        self.detail_command = None
        self.detail_last_update = 0
        self.previous_scroll_anchor = None
        self.selected_name = None
        self.metric_history = {}
        self.kill_prompt = None
        self.kill_prompt_error = None

    def set_lazy_window(self, size):
        """Adjust the number of samples used for lazy averaging."""
        size = max(1, min(size, 50))
        if size == self.lazy_window:
            return
        self.lazy_window = size
        for history in self.metric_history.values():
            history['cpu'] = deque(list(history['cpu'])[-size:], maxlen=size)
            history['mem'] = deque(list(history['mem'])[-size:], maxlen=size)

    def get_processes(self):
        """Get process list and sum by process name"""
        try:
            # Put comm as last column so it gets the remaining width
            result = subprocess.run(
                ['ps', '-caxo', '%cpu=,rss=,comm='],
                capture_output=True,
                text=True,
                timeout=1
            )
            
            processes = defaultdict(lambda: {'cpu': 0.0, 'mem': 0.0, 'count': 0})
            
            for line in result.stdout.strip().split('\n'):
                if not line.strip():
                    continue
                    
                parts = line.split(None, 2)  # Split into 3 parts: cpu, mem, command
                if len(parts) < 3:
                    continue
                
                cpu_str, mem_str, command = parts
                
                try:
                    cpu = float(cpu_str)
                    mem = float(mem_str)
                except ValueError:
                    continue
                
                # Command is already just the executable name thanks to -c flag
                processes[command]['cpu'] += cpu
                processes[command]['mem'] += mem
                processes[command]['count'] += 1
            
            # Convert to list and sort
            proc_list = [
                {
                    'name': name,
                    'cpu': data['cpu'],
                    'mem_mb': data['mem'] / 1024.0,
                    'count': data['count']
                }
                for name, data in processes.items()
            ]

            # Update rolling histories and compute lazy metrics
            for proc in proc_list:
                history = self.metric_history.get(proc['name'])
                if history is None:
                    history = {
                        'cpu': deque(maxlen=self.lazy_window),
                        'mem': deque(maxlen=self.lazy_window)
                    }
                    self.metric_history[proc['name']] = history
                else:
                    if history['cpu'].maxlen != self.lazy_window:
                        history['cpu'] = deque(list(history['cpu'])[-self.lazy_window:], maxlen=self.lazy_window)
                    if history['mem'].maxlen != self.lazy_window:
                        history['mem'] = deque(list(history['mem'])[-self.lazy_window:], maxlen=self.lazy_window)
                history['cpu'].append(proc['cpu'])
                history['mem'].append(proc['mem_mb'])
                if history['cpu']:
                    lazy_cpu = statistics.median(history['cpu'])
                else:
                    lazy_cpu = proc['cpu']
                if history['mem']:
                    lazy_mem = statistics.median(history['mem'])
                else:
                    lazy_mem = proc['mem_mb']
                proc['cpu_lazy'] = lazy_cpu
                proc['mem_lazy'] = lazy_mem
            
            # Clean up history entries for processes no longer present
            current_names = {proc['name'] for proc in proc_list}
            stale = [name for name in self.metric_history.keys() if name not in current_names]
            for name in stale:
                del self.metric_history[name]
            
            # Sort by CPU, Memory, or Name (lazy metrics when enabled for cpu/mem)
            if self.sort_by == 'cpu':
                sort_key = 'cpu_lazy' if self.lazy_mode else 'cpu'
                proc_list.sort(key=lambda x: x[sort_key], reverse=self.sort_descending)
            elif self.sort_by == 'mem':
                sort_key = 'mem_lazy' if self.lazy_mode else 'mem_mb'
                proc_list.sort(key=lambda x: x[sort_key], reverse=self.sort_descending)
            else:  # name
                proc_list.sort(key=lambda x: x['name'].lower(), reverse=self.sort_descending)
            
            return proc_list
            
        except Exception as e:
            return []

    def update_process_details(self, command):
        """Update cached per-PID details for the selected command."""
        current_time = time.time() * 1000
        if not command:
            self.detail_info = []
            self.detail_command = None
            return
        if (command == self.detail_command and
                current_time - self.detail_last_update < self.poll_interval):
            return
        try:
            result = subprocess.run(
                ['ps', '-caxo', 'pid=,%cpu=,rss=,comm='],
                capture_output=True,
                text=True,
                timeout=1
            )
            details = []
            for line in result.stdout.strip().split('\n'):
                if not line.strip():
                    continue
                parts = line.split(None, 3)
                if len(parts) < 4:
                    continue
                pid_str, cpu_str, mem_str, comm = parts
                if comm != command:
                    continue
                try:
                    pid = int(pid_str)
                    cpu = float(cpu_str)
                    mem_mb = float(mem_str) / 1024.0
                except ValueError:
                    continue
                details.append({
                    'pid': pid,
                    'cpu': cpu,
                    'mem_mb': mem_mb,
                    'name': comm
                })
            details.sort(key=lambda x: x['cpu'], reverse=True)
            self.detail_info = details
        except Exception:
            self.detail_info = []
        self.detail_command = command
        self.detail_last_update = current_time

    def get_pids_for_command(self, command):
        """Return list of PIDs for a given command by refreshing details."""
        if not command:
            return []
        self.detail_command = None
        self.detail_last_update = 0
        self.update_process_details(command)
        return [info['pid'] for info in self.detail_info if info['name'] == command]


def draw_ui(stdscr, monitor):
    """Draw the UI"""
    curses.curs_set(0)  # Hide cursor
    stdscr.nodelay(1)  # Non-blocking input
    stdscr.timeout(10)  # 10ms timeout for getch() - very responsive
    stdscr.keypad(True)  # Enable special keys like arrows
    
    # Use default colors (transparent background)
    curses.use_default_colors()
    
    # Color pairs with transparent background (-1)
    curses.init_pair(1, curses.COLOR_CYAN, -1)
    curses.init_pair(2, curses.COLOR_GREEN, -1)
    curses.init_pair(3, curses.COLOR_YELLOW, -1)
    curses.init_pair(4, curses.COLOR_RED, -1)
    curses.init_pair(5, curses.COLOR_WHITE, -1)  # Modal text with transparent background
    
    # ASCII logo
    logo = [
        "     _ _              ",
        "  __| | |_ ___  _ __  ",
        " / _` | __/ _ \\| '_ \\ ",
        "| (_| | || (_) | |_) |",
        " \\__,_|\\__\\___/| .__/ ",
        "    @damsleth's|_|top "
    ]
    
    while monitor.running:
        try:
            height, width = stdscr.getmaxyx()
            
            # Check if it's time to update process data
            current_time = time.time() * 1000  # Convert to milliseconds
            if current_time - monitor.last_update >= monitor.poll_interval:
                monitor.current_processes = monitor.get_processes()
                monitor.last_update = current_time
            
            processes = monitor.current_processes
            
            # Apply search filter
            query = monitor.search_query.lower()
            if query:
                filtered = [p for p in processes if query in p['name'].lower()]
            else:
                filtered = list(processes)
            monitor.filtered_processes = filtered
            
            # Helper to safely draw strings without triggering curses ERR on edges
            def draw_text(y, x, text, max_len, attr=0, pad=True):
                if max_len <= 0 or y < 0 or y >= height:
                    return
                if x < 0 or x >= width:
                    return
                available = width - x
                if available <= 0:
                    return
                length = min(max_len, available)
                if y == height - 1:
                    length = min(length, max(0, width - x - 1))
                if length <= 0:
                    return
                snippet = text[:length]
                if pad:
                    snippet = snippet.ljust(length)
                try:
                    stdscr.addnstr(y, x, snippet, length, attr)
                except curses.error:
                    pass
            
            # Keep selection within bounds and anchored by command name if extended view is open
            if not filtered:
                monitor.selected_index = None
                monitor.selected_name = None
                monitor.show_extended = False
                monitor.previous_scroll_anchor = None
                monitor.detail_info = []
                monitor.detail_command = None
            else:
                if monitor.show_extended and monitor.selected_name:
                    new_index = next((i for i, proc in enumerate(filtered)
                                      if proc['name'] == monitor.selected_name), None)
                    if new_index is not None:
                        monitor.selected_index = new_index
                    else:
                        monitor.show_extended = False
                        monitor.detail_info = []
                        monitor.detail_command = None
                        monitor.selected_name = None
                        monitor.selected_index = None
                if monitor.selected_index is not None:
                    monitor.selected_index = max(0, min(monitor.selected_index, len(filtered) - 1))
            selection_exists = bool(filtered) and monitor.selected_index is not None
            
            selected_name = None
            if monitor.show_extended and monitor.selected_name:
                selected_name = monitor.selected_name
                monitor.update_process_details(selected_name)
            
            stdscr.clear()
            value_width = 12 if monitor.lazy_mode else 10
            name_col_width = max(1, width - (value_width * 2 + 2))
            
            # Optional search bar
            search_bar_visible = monitor.search_mode or bool(monitor.search_query)
            row_cursor = 0
            if search_bar_visible and row_cursor < height:
                search_label = "Search: "
                display_query = monitor.search_query
                if monitor.search_mode:
                    display_query += "_"
                search_str = f"{search_label}{display_query}"
                attr = curses.color_pair(2) | (curses.A_BOLD if monitor.search_mode else 0)
                draw_text(row_cursor, 0, search_str, width, attr)
                row_cursor += 1
            
            # Column headers with active sort highlight
            header_y = row_cursor
            if header_y < height:
                base_attr = curses.A_BOLD | curses.A_UNDERLINE
                name_attr = base_attr | (curses.A_REVERSE if monitor.sort_by == 'name' else 0)
                cpu_attr = base_attr | (curses.A_REVERSE if monitor.sort_by == 'cpu' else 0)
                mem_attr = base_attr | (curses.A_REVERSE if monitor.sort_by == 'mem' else 0)
                direction_icon = '↓' if monitor.sort_descending else '↑'
                name_label = f"{direction_icon} PROCESS NAME" if monitor.sort_by == 'name' else "PROCESS NAME"
                cpu_label = f"{direction_icon} CPU%" if monitor.sort_by == 'cpu' else "CPU%"
                mem_label = f"{direction_icon} MEM(MB)" if monitor.sort_by == 'mem' else "MEM(MB)"
                if monitor.lazy_mode:
                    cpu_label += " x̄"
                    mem_label += " x̄"
                draw_text(header_y, 0, name_label[:min(name_col_width, width)].ljust(min(name_col_width, width)), min(name_col_width, width), name_attr)
                cpu_x = name_col_width + 1
                if cpu_x < width:
                    cpu_width = min(value_width, width - cpu_x)
                    draw_text(header_y, cpu_x, cpu_label[:value_width].ljust(value_width), cpu_width, cpu_attr, pad=False)
                mem_x = name_col_width + 1 + value_width + 1
                if mem_x < width:
                    mem_width = min(value_width, width - mem_x)
                    draw_text(header_y, mem_x, mem_label[:value_width].ljust(value_width), mem_width, mem_attr, pad=False)
            
            table_start_row = header_y + 1
            available_space = max(0, height - table_start_row)
            
            # Reserve space for extended details when requested
            detail_active = monitor.show_extended and selected_name and available_space > 0
            detail_height = 0
            if detail_active:
                detail_rows = len(monitor.detail_info)
                # Total rows needed: title + optional header + detail rows (or a placeholder)
                required_rows = 1  # title row
                if detail_rows > 0:
                    required_rows += 1 + detail_rows  # header + each PID
                else:
                    required_rows += 1  # placeholder line
                requested = max(5, required_rows)
                max_height = max(0, available_space - 1)  # leave room for the separator
                detail_height = min(requested, max_height)
                if detail_height <= 0:
                    detail_active = False
                    detail_height = 0
            
            visible_rows = available_space if not detail_active else max(0, available_space - detail_height - 1)
            
            # Determine scrolling window start index
            start_index = 0
            if selection_exists and visible_rows > 0:
                if monitor.previous_scroll_anchor is None:
                    monitor.previous_scroll_anchor = 0
                start_index = monitor.previous_scroll_anchor
                if monitor.selected_index < start_index:
                    start_index = monitor.selected_index
                elif monitor.selected_index >= start_index + visible_rows:
                    start_index = monitor.selected_index - visible_rows + 1
                start_index = max(0, min(start_index, max(0, len(filtered) - visible_rows)))
                monitor.previous_scroll_anchor = start_index
            elif visible_rows > 0:
                monitor.previous_scroll_anchor = 0
            
            # Process list
            for i in range(visible_rows):
                idx = start_index + i
                if idx >= len(filtered):
                    break
                proc = filtered[idx]
                
                display_name = proc['name']
                process_count = proc.get('count', 1)
                if process_count > 1:
                    display_name = f"{display_name} ({process_count})"
                proc_name = display_name[:name_col_width]
                display_cpu = proc['cpu_lazy'] if monitor.lazy_mode else proc['cpu']
                display_mem = proc['mem_lazy'] if monitor.lazy_mode else proc['mem_mb']
                line = f"{proc_name:<{name_col_width}} {display_cpu:>{value_width}.2f} {display_mem:>{value_width}.2f}"
                
                # Color code based on CPU usage
                color = curses.color_pair(0)
                if display_cpu > 50:
                    color = curses.color_pair(4)  # Red for high CPU
                elif display_cpu > 20:
                    color = curses.color_pair(3)  # Yellow for medium CPU
                
                row_attr = color
                if monitor.selected_index == idx or (
                    monitor.show_extended and monitor.selected_name == proc['name']
                ):
                    row_attr |= curses.A_REVERSE
                
                row_y = table_start_row + i
                if row_y >= height:
                    break
                draw_text(row_y, 0, line, width, row_attr, pad=False)
            
            # Extended info split
            if detail_active and detail_height > 0 and selected_name:
                details_start = height - detail_height
                separator_y = details_start - 1
                if separator_y >= table_start_row and separator_y < height:
                    draw_text(separator_y, 0, "-" * width, width, curses.color_pair(0), pad=False)
                
                title = f"Extended Info: {selected_name}"
                draw_text(details_start, 0, title, width, curses.color_pair(1) | curses.A_BOLD)
                
                detail_cursor = details_start + 1
                remaining_rows = detail_height - 1
                
                if remaining_rows > 0 and detail_cursor < height:
                    header_attr = curses.color_pair(5) | curses.A_BOLD
                    draw_text(detail_cursor, 0, "PID       CPU%      MEM(MB)", width, header_attr)
                    detail_cursor += 1
                    remaining_rows -= 1
                
                if remaining_rows > 0:
                    if monitor.detail_info:
                        for info in monitor.detail_info[:remaining_rows]:
                            if detail_cursor >= height:
                                break
                            pid_text = f"{info['pid']:>6}"
                            cpu_text = f"{info['cpu']:>6.2f}"
                            mem_text = f"{info['mem_mb']:>7.2f}"
                            info_line = f"{pid_text}   {cpu_text}     {mem_text}"
                            draw_text(detail_cursor, 0, info_line, width, curses.color_pair(0))
                            detail_cursor += 1
                    else:
                        draw_text(detail_cursor, 0, "No processes found", width, curses.color_pair(0))
            
            # Draw help modal if active
            if monitor.show_help:
                draw_help_modal(stdscr, monitor, logo, height, width)
            
            # Draw kill confirmation modal if active
            if monitor.kill_prompt:
                draw_kill_modal(stdscr, monitor, height, width)
            
            stdscr.refresh()
            
            # Handle input
            processed_key = False
            try:
                key = stdscr.getch()
                if key != -1:
                    processed_key = True
                    if monitor.kill_prompt:
                        if key in (27,):
                            monitor.kill_prompt = None
                            monitor.kill_prompt_error = None
                        elif key in (10, 13, curses.KEY_ENTER):
                            errors = []
                            for pid in monitor.kill_prompt.get('pids', []):
                                try:
                                    os.kill(pid, signal.SIGKILL)
                                except Exception as exc:
                                    errors.append(str(exc))
                            if errors:
                                monitor.kill_prompt_error = errors[0][:width-6]
                            else:
                                monitor.kill_prompt = None
                                monitor.kill_prompt_error = None
                                monitor.last_update = 0
                        continue
                    if monitor.search_mode:
                        if key in (27,):  # ESC exits search and clears filter
                            monitor.search_mode = False
                            monitor.search_query = ''
                            monitor.selected_index = None
                            monitor.show_extended = False
                            monitor.previous_scroll_anchor = None
                            monitor.detail_info = []
                            monitor.detail_command = None
                            monitor.selected_name = None
                        elif key in (10, 13, curses.KEY_ENTER):
                            monitor.search_mode = False
                        elif key in (curses.KEY_BACKSPACE, 127, 8):
                            if monitor.search_query:
                                monitor.search_query = monitor.search_query[:-1]
                                monitor.selected_index = None
                                monitor.show_extended = False
                                monitor.previous_scroll_anchor = None
                                monitor.detail_info = []
                                monitor.detail_command = None
                                monitor.selected_name = None
                        elif 32 <= key <= 126:
                            monitor.search_query += chr(key)
                            monitor.selected_index = None
                            monitor.show_extended = False
                            monitor.previous_scroll_anchor = None
                            monitor.detail_info = []
                            monitor.detail_command = None
                            monitor.selected_name = None
                        elif key in (curses.KEY_DOWN,):
                            if monitor.filtered_processes:
                                if monitor.selected_index is None:
                                    monitor.selected_index = 0
                                else:
                                    monitor.selected_index = min(
                                        monitor.selected_index + 1,
                                        len(monitor.filtered_processes) - 1
                                    )
                                if monitor.show_extended:
                                    monitor.selected_name = monitor.filtered_processes[monitor.selected_index]['name']
                                    monitor.update_process_details(monitor.selected_name)
                                else:
                                    monitor.selected_name = None
                        elif key in (curses.KEY_UP,):
                            if monitor.filtered_processes:
                                if monitor.selected_index is None:
                                    monitor.selected_index = len(monitor.filtered_processes) - 1
                                else:
                                    monitor.selected_index = max(monitor.selected_index - 1, 0)
                                if monitor.show_extended:
                                    monitor.selected_name = monitor.filtered_processes[monitor.selected_index]['name']
                                    monitor.update_process_details(monitor.selected_name)
                                else:
                                    monitor.selected_name = None
                    else:
                        sort_cycle = ['cpu', 'mem', 'name']

                        def change_sort(target):
                            if monitor.sort_by != target:
                                monitor.sort_by = target
                                monitor.last_update = 0

                        if key in (27, ord('?')):  # ESC or ? toggles help
                            monitor.show_help = not monitor.show_help
                        elif key == ord('q') or key == ord('Q'):
                            monitor.running = False
                        elif key == ord('+') or key == ord('='):
                            monitor.poll_interval = min(10000, monitor.poll_interval + 100)
                        elif key == ord('-') or key == ord('_'):
                            monitor.poll_interval = max(100, monitor.poll_interval - 100)
                        elif key == ord('x') or key == ord('X'):
                            monitor.lazy_mode = not monitor.lazy_mode
                        elif key == ord('r') or key == ord('R'):
                            monitor.sort_descending = not monitor.sort_descending
                            monitor.last_update = 0
                        elif key == ord('k') or key == ord('K'):
                            if monitor.filtered_processes and monitor.selected_index is not None:
                                target_name = monitor.filtered_processes[monitor.selected_index]['name']
                                pids = monitor.get_pids_for_command(target_name)
                                if pids:
                                    monitor.kill_prompt = {'command': target_name, 'pids': pids}
                                    monitor.kill_prompt_error = None
                                else:
                                    monitor.kill_prompt = {'command': target_name, 'pids': []}
                                    monitor.kill_prompt_error = "No PIDs found"
                        elif key == ord('c') or key == ord('C'):
                            change_sort('cpu')
                        elif key == ord('m') or key == ord('M'):
                            change_sort('mem')
                        elif key == ord('p') or key == ord('P'):
                            change_sort('name')
                        elif key == curses.KEY_LEFT:
                            if monitor.sort_by in sort_cycle:
                                idx = sort_cycle.index(monitor.sort_by)
                                change_sort(sort_cycle[(idx - 1) % len(sort_cycle)])
                            else:
                                change_sort('cpu')
                        elif key == curses.KEY_RIGHT:
                            if monitor.sort_by in sort_cycle:
                                idx = sort_cycle.index(monitor.sort_by)
                                change_sort(sort_cycle[(idx + 1) % len(sort_cycle)])
                            else:
                                change_sort('cpu')
                        elif key == ord('.'): 
                            monitor.set_lazy_window(monitor.lazy_window + 1)
                        elif key == ord(','):
                            monitor.set_lazy_window(monitor.lazy_window - 1)
                        elif key == curses.KEY_NPAGE:
                            if monitor.filtered_processes:
                                if monitor.selected_index is None:
                                    monitor.selected_index = min(visible_rows - 1, len(monitor.filtered_processes) - 1)
                                else:
                                    monitor.selected_index = min(
                                        monitor.selected_index + max(1, visible_rows),
                                        len(monitor.filtered_processes) - 1
                                    )
                                if monitor.show_extended and monitor.selected_index is not None:
                                    monitor.selected_name = monitor.filtered_processes[monitor.selected_index]['name']
                                    monitor.update_process_details(monitor.selected_name)
                                else:
                                    monitor.selected_name = None
                        elif key == curses.KEY_PPAGE:
                            if monitor.filtered_processes:
                                if monitor.selected_index is None:
                                    monitor.selected_index = 0
                                else:
                                    monitor.selected_index = max(
                                        monitor.selected_index - max(1, visible_rows),
                                        0
                                    )
                                if monitor.show_extended and monitor.selected_index is not None:
                                    monitor.selected_name = monitor.filtered_processes[monitor.selected_index]['name']
                                    monitor.update_process_details(monitor.selected_name)
                                else:
                                    monitor.selected_name = None
                        elif key == curses.KEY_DOWN:
                            if monitor.filtered_processes:
                                if monitor.selected_index is None:
                                    monitor.selected_index = 0
                                else:
                                    monitor.selected_index = min(
                                        monitor.selected_index + 1,
                                        len(monitor.filtered_processes) - 1
                                    )
                                if monitor.show_extended and monitor.selected_index is not None:
                                    monitor.selected_name = monitor.filtered_processes[monitor.selected_index]['name']
                                    monitor.update_process_details(monitor.selected_name)
                                else:
                                    monitor.selected_name = None
                        elif key == curses.KEY_UP:
                            if monitor.filtered_processes:
                                if monitor.selected_index is None:
                                    monitor.selected_index = len(monitor.filtered_processes) - 1
                                else:
                                    monitor.selected_index = max(monitor.selected_index - 1, 0)
                                if monitor.show_extended and monitor.selected_index is not None:
                                    monitor.selected_name = monitor.filtered_processes[monitor.selected_index]['name']
                                    monitor.update_process_details(monitor.selected_name)
                                else:
                                    monitor.selected_name = None
                        elif key in (10, 13, curses.KEY_ENTER):
                            if monitor.selected_index is not None and monitor.filtered_processes:
                                selected = monitor.filtered_processes[monitor.selected_index]['name']
                                if monitor.show_extended and monitor.detail_command == selected:
                                    monitor.show_extended = False
                                    monitor.detail_info = []
                                    monitor.detail_command = None
                                    monitor.selected_name = None
                                else:
                                    monitor.show_extended = True
                                    monitor.selected_name = selected
                                    monitor.update_process_details(selected)
                        elif key == ord('f') or key == ord('F'):
                            monitor.search_mode = True
            except:
                pass
            
            # Small sleep to prevent busy-waiting
            if not processed_key:
                time.sleep(0.01)
            
        except KeyboardInterrupt:
            monitor.running = False
            break
        except Exception as e:
            # On error, show it briefly then continue
            if height > 2:
                error_msg = f"Error: {str(e)}"[:width-1]
                stdscr.addstr(height-1, 0, error_msg, curses.color_pair(4))
                stdscr.refresh()
            time.sleep(1)


def draw_rounded_box(stdscr, y, x, height, width, border_attr, fill_attr):
    """Draw a rounded border box with filled background."""
    if height < 2 or width < 2:
        return
    try:
        # Corners
        stdscr.addstr(y, x, "╭", border_attr)
        stdscr.addstr(y, x + width - 1, "╮", border_attr)
        stdscr.addstr(y + height - 1, x, "╰", border_attr)
        stdscr.addstr(y + height - 1, x + width - 1, "╯", border_attr)
        # Horizontal edges
        if width > 2:
            horiz = "─" * (width - 2)
            stdscr.addstr(y, x + 1, horiz, border_attr)
            stdscr.addstr(y + height - 1, x + 1, horiz, border_attr)
        # Vertical edges and fill
        if height > 2:
            for row in range(y + 1, y + height - 1):
                stdscr.addstr(row, x, "│", border_attr)
                stdscr.addstr(row, x + width - 1, "│", border_attr)
                if width > 2:
                    stdscr.addstr(row, x + 1, " " * (width - 2), fill_attr)
    except curses.error:
        pass


def draw_help_modal(stdscr, monitor, logo, height, width):
    """Draw the help modal overlay"""
    # Dim the background by drawing a semi-transparent overlay
    try:
        for y in range(height):
            for x in range(width):
                try:
                    ch = stdscr.inch(y, x)
                    stdscr.addch(y, x, ch, curses.A_DIM)
                except:
                    pass
    except:
        pass
    
    # Calculate modal dimensions
    modal_width = min(60, width - 4)
    shortcuts = [
        ("+/-", "Adjust polling interval"),
        ("↑/↓", "Select process row"),
        ("x", "Toggle average mode"),
        (",/.", "Adjust average sample size"),
        ("←/→", "Switch sort column"),
        ("enter", "Toggle extended info"),
        ("c", "Sort by CPU"),
        ("m", "Sort by Memory"),
        ("p", "Sort by Name"),
        ("r", "Reverse sort"),
        ("f", "Filter processes"),
        ("k", "Kill highlighted process"),
        ("esc/?", "Help or close search"),
        ("q", "Quit")
    ]
    
    # Status info
    sort_label = monitor.sort_by.upper()
    if monitor.lazy_mode:
        sort_label += " average (x̄)"
    status_lines = [
      f"Poll interval: {monitor.poll_interval}ms",
      f"Sort: {sort_label}",
      f"# Average Samples: {monitor.lazy_window}"
    ]
    
    content_height = (
        len(logo) +
        2 +
        len(status_lines) +
        2 +
        len(shortcuts) + 1
    )
    modal_height = min(max(12, content_height), max(4, height - 4))
    modal_y = (height - modal_height) // 2
    modal_x = (width - modal_width) // 2
    
    border_attr = curses.color_pair(5)
    draw_rounded_box(stdscr, modal_y, modal_x, modal_height, modal_width, border_attr, curses.color_pair(5))

    inner_x = modal_x + 1
    inner_y = modal_y + 1
    inner_width = modal_width - 2
    inner_height = modal_height - 2
    logo_start_y = inner_y + 1
    try:
        for i, line in enumerate(logo):
            row_y = logo_start_y + i
            if row_y >= inner_y + inner_height:
                break
            line_text = line[:inner_width]
            logo_x = inner_x + max(0, (inner_width - len(line_text)) // 2)
            stdscr.addstr(row_y, logo_x, line_text, curses.color_pair(1) | curses.A_BOLD)
        info_y = logo_start_y + len(logo) + 1
        for i, line in enumerate(status_lines):
            row_y = info_y + i
            if row_y >= inner_y + inner_height:
                break
            text = line[:inner_width]
            line_x = inner_x + max(0, (inner_width - len(text)) // 2)
            stdscr.addstr(row_y, line_x, text, curses.color_pair(2))

        shortcuts_y = info_y + len(status_lines) + 1
        max_key_len = max((len(key) for key, _ in shortcuts), default=0)
        padding = 2
        available_width = max(0, inner_width - padding * 2)
        key_col_width = min(max_key_len, max(1, available_width // 3))
        desc_col_width = max(0, available_width - key_col_width - 2)
        table_width = key_col_width + (2 if desc_col_width > 0 else 0) + desc_col_width
        table_start_x = inner_x + padding + max(0, (available_width - table_width) // 2)
        desc_start_x = table_start_x + key_col_width + (2 if desc_col_width > 0 else 0)

        for i, (key, desc) in enumerate(shortcuts):
            row_y = shortcuts_y + i
            if row_y >= inner_y + inner_height:
                break
            key_text = key[:key_col_width].ljust(key_col_width)
            stdscr.addstr(row_y, table_start_x, key_text, curses.color_pair(0))
            if desc_col_width > 0:
                desc_text = desc[:desc_col_width].ljust(desc_col_width)
                stdscr.addstr(row_y, desc_start_x, desc_text, curses.color_pair(0))
    except curses.error:
        pass


def draw_kill_modal(stdscr, monitor, height, width):
    """Draw confirmation modal for SIGKILL."""
    prompt = monitor.kill_prompt
    if not prompt:
        return
    command = prompt.get('command', '')
    pids = prompt.get('pids', [])
    
    lines = ["Send SIGKILL?", f"Process: {command}"]
    if pids:
        prefix = "PIDs: "
        current = prefix
        max_content_width = max(10, min(width - 6, 50))
        for pid in pids:
            pid_str = str(pid)
            sep = "" if current.endswith(": ") else ", "
            candidate = current + sep + pid_str
            if len(candidate) > max_content_width:
                lines.append(current)
                current = "      " + pid_str
            else:
                current = candidate
        lines.append(current)
    else:
        lines.append("No PIDs found")
    
    lines.append("Enter = confirm   Esc = cancel")
    if monitor.kill_prompt_error:
        lines.append(f"Error: {monitor.kill_prompt_error}")
    
    content_width = max(len(line) for line in lines)
    modal_width = min(max(content_width + 4, 24), max(10, width - 4))
    modal_height = min(len(lines) + 4, max(6, height - 4))
    modal_y = (height - modal_height) // 2
    modal_x = (width - modal_width) // 2
    draw_rounded_box(stdscr, modal_y, modal_x, modal_height, modal_width, curses.color_pair(5), curses.color_pair(5))
    inner_x = modal_x + 1
    inner_y = modal_y + 1
    inner_width = modal_width - 2
    inner_height = modal_height - 2
    try:
        for i, line in enumerate(lines[:inner_height]):
            row_y = inner_y + i
            text = line[:inner_width].ljust(inner_width)
            stdscr.addstr(row_y, inner_x, text, curses.color_pair(0))
    except curses.error:
        pass


def main():
    """Main entry point"""
    # Set ESC delay to 0 BEFORE initializing curses
    import os
    os.environ['ESCDELAY'] = '0'
    
    monitor = ProcessMonitor()
    
    # Handle Ctrl+C gracefully
    def signal_handler(sig, frame):
        monitor.running = False
    
    signal.signal(signal.SIGINT, signal_handler)
    
    try:
        curses.wrapper(lambda stdscr: draw_ui(stdscr, monitor))
    except Exception as e:
        print(f"Error starting dtop: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == '__main__':
    main()
