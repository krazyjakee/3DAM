(() => {
  // Capture constructors, methods, and Web-IDL getters before page code can replace prototypes.
  // Bound originals are essential: calling `headers.set(...)` later would let an XSS monkeypatch
  // Headers.prototype.set and observe the bearer value.
  const NativeHeaders = Headers;
  const NativeRequest = Request;
  const NativeURL = URL;
  const nativeFetch = window.fetch.bind(window);
  const call = Function.prototype.call;
  const bindCall = (fn) => call.bind(fn);
  const headersSet = bindCall(NativeHeaders.prototype.set);
  const headersHas = bindCall(NativeHeaders.prototype.has);
  const headersForEach = bindCall(NativeHeaders.prototype.forEach);
  const requestUrl = bindCall(Object.getOwnPropertyDescriptor(NativeRequest.prototype, "url").get);
  const requestHeaders = bindCall(
    Object.getOwnPropertyDescriptor(NativeRequest.prototype, "headers").get,
  );
  const urlOrigin = bindCall(Object.getOwnPropertyDescriptor(NativeURL.prototype, "origin").get);
  const urlPathname = bindCall(
    Object.getOwnPropertyDescriptor(NativeURL.prototype, "pathname").get,
  );
  const isRequest = Function.prototype[Symbol.hasInstance].bind(NativeRequest);
  const locationHref = String(location.href);
  const credential = __3DAM_CREDENTIAL_JSON__;
  const allowedOrigin = __3DAM_ORIGIN_JSON__;
  const allowedPath = __3DAM_PATH_JSON__;
  const nativeBase = __3DAM_BASE_JSON__;
  const credentialForgettable = __3DAM_FORGETTABLE_JSON__;

  Object.defineProperty(window, "__3DAM_NATIVE_CREDENTIAL__", { value: true });
  Object.defineProperty(window, "__3DAM_NATIVE_BASE__", { value: nativeBase });
  Object.defineProperty(window, "__3DAM_NATIVE_CREDENTIAL_FORGETTABLE__", {
    value: credentialForgettable,
  });
  window.fetch = (input, init = {}) => {
    const request = isRequest(input) ? input : null;
    const target = new NativeURL(request ? requestUrl(request) : String(input), locationHref);
    const pathname = urlPathname(target);
    const mount = allowedPath.replace(/\/$/, "") || "/";
    const inMount = mount === "/" || pathname === mount || pathname.startsWith(`${mount}/`);
    if (urlOrigin(target) !== allowedOrigin || !inMount) return nativeFetch(input, init);

    const headers = new NativeHeaders(request ? requestHeaders(request) : undefined);
    const additions = new NativeHeaders(init.headers);
    headersForEach(additions, (value, name) => headersSet(headers, name, value));
    if (!headersHas(headers, "authorization")) {
      headersSet(headers, "authorization", `Bearer ${credential}`);
    }
    return nativeFetch(
      request ? new NativeRequest(request, { ...init, headers }) : input,
      request ? undefined : { ...init, headers },
    );
  };
})();
