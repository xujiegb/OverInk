// SPDX-License-Identifier: BSD-3-Clause
// Rust supplies this process's PID, the reply bus address and the operation.
(function () {
    const windows = workspace.stackingOrder;
    const title = placeToolbar ? "OverInk" : "OverInk - canvas";
    let result = null;
    for (let i = 0; i < windows.length; ++i) {
        const window = windows[i];
        if (window.pid !== overinkPid ||
            (window.captionNormal || window.caption) !== title) {
            continue;
        }
        if (placeToolbar) {
            const area = workspace.clientArea(KWin.MaximizeArea, window);
            const frame = window.frameGeometry;
            if (area.width <= 0 || area.height <= 0 ||
                frame.width <= 0 || frame.height <= 0) {
                break;
            }
            frame.x = area.x;
            frame.y = area.y + Math.max(0, (area.height - frame.height) / 2);
            window.frameGeometry = frame;
        }
        const geometry = window.clientGeometry;
        result = [geometry.x, geometry.y, geometry.width, geometry.height];
        break;
    }
    callDBus(replyDestination, "/org/overink/KWinGeometry",
             "org.overink.KWinGeometry", "Report", JSON.stringify(result));
})();
