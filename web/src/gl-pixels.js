// OpenGL 4.4 uncompressed pixel-transfer span calculation. This mirrors
// guest/gl/gexgl-pixel.h; both sides must agree before either touches memory.

export function defaultPixelStore() {
  return { alignment: 4, rowLength: 0, imageHeight: 0,
           skipPixels: 0, skipRows: 0, skipImages: 0 };
}

const mul = (a, b) => {
  const value = a * b;
  return Number.isSafeInteger(value) ? value : null;
};
const add = (a, b) => {
  const value = a + b;
  return Number.isSafeInteger(value) ? value : null;
};

function components(format) {
  switch (format) {
    case 0x1900: case 0x1901: case 0x1902: case 0x1903:
    case 0x1904: case 0x1905: case 0x1906: case 0x1909:
    case 0x8d94: case 0x8d95: case 0x8d96: case 0x84f9: return 1;
    case 0x190a: case 0x8227: case 0x8228: return 2;
    case 0x1907: case 0x80e0: case 0x8d98: case 0x8d9a: return 3;
    case 0x1908: case 0x80e1: case 0x8d99: case 0x8d9b: return 4;
    default: return 0;
  }
}

function groupBytes(format, type) {
  switch (type) {
    case 0x8032: case 0x8362: return 1;
    case 0x8033: case 0x8034: case 0x8363: case 0x8364:
    case 0x8365: case 0x8366: return 2;
    case 0x8035: case 0x8036: case 0x8367: case 0x8368:
    case 0x84fa: case 0x8c3b: case 0x8c3e: return 4;
    case 0x8dad: return 8;
  }
  const scalar = type === 0x1400 || type === 0x1401 ? 1
    : type === 0x1402 || type === 0x1403 || type === 0x140b ? 2
    : type === 0x1404 || type === 0x1405 || type === 0x1406 || type === 0x140c ? 4
    : type === 0x140a ? 8 : 0;
  return scalar * components(format);
}

// depth === null means 2D; a non-negative number means 3D.
export function pixelLayout(format, type, width, height, depth, store) {
  const is3d = depth !== null;
  if (!Number.isInteger(width) || !Number.isInteger(height) || width < 0 || height < 0 ||
      (is3d && (!Number.isInteger(depth) || depth < 0))) return null;
  if (!width || !height || (is3d && !depth))
    return { offset: 0, rowStride: 0, imageStride: 0, end: 0 };
  if (type === 0x1a00) return null; // GL_BITMAP needs bit-level transport.
  const group = groupBytes(format, type);
  if (!group) return null;
  const rowBytes = mul(store.rowLength || width, group);
  if (rowBytes === null) return null;
  const rowStride = Math.ceil(rowBytes / store.alignment) * store.alignment;
  if (!Number.isSafeInteger(rowStride)) return null;
  const imageStride = mul(rowStride, is3d && store.imageHeight ? store.imageHeight : height);
  const oi = mul(is3d ? store.skipImages : 0, imageStride);
  const or = mul(store.skipRows, rowStride);
  const op = mul(store.skipPixels, group);
  if ([imageStride, oi, or, op].some((value) => value === null)) return null;
  let offset = add(oi, or); offset = offset === null ? null : add(offset, op);
  const li = mul((is3d ? depth : 1) - 1, imageStride);
  const lr = mul(height - 1, rowStride);
  const active = mul(width, group);
  if ([offset, li, lr, active].some((value) => value === null)) return null;
  let end = add(offset, li); end = end === null ? null : add(end, lr);
  end = end === null ? null : add(end, active);
  return end === null ? null : { offset, rowStride, imageStride, end };
}

export function setPixelStore(pack, unpack, pname, param) {
  if (!Number.isInteger(param) || param < 0) return false;
  let state, field;
  switch (pname) {
    case 0x0d05: state = pack; field = 'alignment'; break;
    case 0x0d02: state = pack; field = 'rowLength'; break;
    case 0x806c: state = pack; field = 'imageHeight'; break;
    case 0x0d04: state = pack; field = 'skipPixels'; break;
    case 0x0d03: state = pack; field = 'skipRows'; break;
    case 0x806b: state = pack; field = 'skipImages'; break;
    case 0x0cf5: state = unpack; field = 'alignment'; break;
    case 0x0cf2: state = unpack; field = 'rowLength'; break;
    case 0x806e: state = unpack; field = 'imageHeight'; break;
    case 0x0cf4: state = unpack; field = 'skipPixels'; break;
    case 0x0cf3: state = unpack; field = 'skipRows'; break;
    case 0x806d: state = unpack; field = 'skipImages'; break;
    default: return false;
  }
  if (field === 'alignment' && ![1, 2, 4, 8].includes(param)) return false;
  state[field] = param;
  return true;
}
