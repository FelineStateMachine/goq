export function committedRustConnection(result) {
  return result?.connected === true;
}

export async function disconnectRejectedRustConnection(invokeCommand, committed) {
  if (!committed) return false;
  await invokeCommand('iroh_client_disconnect');
  return true;
}
