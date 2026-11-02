// Name under which the Rust host binary is registered with Chrome Native
// Messaging. Chrome spawns the host on demand when the bridge calls
// chrome.runtime.connectNative(NATIVE_HOST_NAME) and tears it down when the
// port disconnects.

export const NATIVE_HOST_NAME = "com.cgpt_bridge.host";
