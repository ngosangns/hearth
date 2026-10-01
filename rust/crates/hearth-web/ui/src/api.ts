export async function api(method: string, path: string, body?: unknown) {
  const response = await fetch(path, {
    method,
    headers: body
      ? { "content-type": "application/json", accept: "application/json" }
      : { accept: "application/json" },
    body: body ? JSON.stringify(body) : undefined,
  });
  const text = await response.text();
  let data: any = {};
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = {};
    }
  }
  if (!response.ok) throw new Error(data.error?.message || `Request failed (${response.status})`);
  return data;
}
