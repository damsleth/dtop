# ---- dtop - @damsleth's Activity Monitor clone for terminal ----- #
# A minimal, interactive process monitor that sums usage by process name
# like macOS Activity Monitor does, giving better visibility into total
# app consumption (especially for multi-process apps like browsers)
#
# Usage: just run `dtop` in terminal

dtop-py() {
    # Get the directory where this script file lives, not where it's called from
    local script_dir="${${(%):-%x}:A:h}"
    local script_path="$script_dir/plugins/dtop.py"

    # Backward compatibility with previous layout
    if [[ ! -f "$script_path" ]]; then
        script_path="$script_dir/dtop.py"
    fi
    
    # Check if Python script exists
    if [[ ! -f "$script_path" ]]; then
        echo "Error: dtop.py not found at $script_path"
        return 1
    fi
    
    # Make sure it's executable
    chmod +x "$script_path" 2>/dev/null
    
    # Run the Python script
    python3 "$script_path"
}

dtop() {
    # Get the directory where this script file lives, not where it's called from
    local script_dir="${${(%):-%x}:A:h}"
    local rust_dir="$script_dir/plugins/dtop"

    # Backward compatibility with previous layout
    if [[ ! -d "$rust_dir" ]]; then
        rust_dir="$script_dir/dtop"
    fi

    local rust_manifest="$rust_dir/Cargo.toml"
    local rust_bin="$rust_dir/target/release/dtop"

    # If Cargo project is missing, fallback to Python implementation
    if [[ ! -f "$rust_manifest" ]]; then
        dtop-py
        return $?
    fi

    # Prefer compiled release binary if available
    if [[ -x "$rust_bin" ]]; then
        "$rust_bin"
        return $?
    fi

    # Otherwise run via cargo; if that fails, fallback to Python
    if command -v cargo >/dev/null 2>&1; then
        (
            cd "$rust_dir" || exit 1
            cargo run
        )
        local status=$?
        if [[ $status -eq 0 ]]; then
            return 0
        fi
        echo "dtop: Rust launch failed, falling back to Python version"
        dtop-py
        return $?
    fi

    echo "dtop: cargo not found, falling back to Python version"
    dtop-py
}