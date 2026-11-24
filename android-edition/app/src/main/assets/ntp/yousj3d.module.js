/* Yousj 3D v0.1.0 — single-file build. Source: src/ (ES modules). MIT. */
// ---- src/math/Color.js ----
/**
 * Color — RGB color, API mirrors three.js.
 * Accepts: hex number (0xff0000), CSS string ('#ff0000' / 'red' subset via hex only),
 * (r, g, b) floats, or another Color.
 */
class Color {
  constructor(r, g, b) {
    this.isColor = true;
    this.r = 1;
    this.g = 1;
    this.b = 1;
    if (r !== undefined) this.set(r, g, b);
  }
  set(r, g, b) {
    if (r instanceof Color) return this.copy(r);
    if (typeof r === 'number' && g === undefined) return this.setHex(r);
    if (typeof r === 'string') return this.setStyle(r);
    this.r = r ?? 1;
    this.g = g ?? 1;
    this.b = b ?? 1;
    return this;
  }
  setHex(hex) {
    hex = Math.floor(hex);
    this.r = ((hex >> 16) & 255) / 255;
    this.g = ((hex >> 8) & 255) / 255;
    this.b = (hex & 255) / 255;
    return this;
  }
  setStyle(style) {
    const m = /^#([0-9a-f]{6})$/i.exec(style.trim());
    if (m) return this.setHex(parseInt(m[1], 16));
    const m3 = /^#([0-9a-f]{3})$/i.exec(style.trim());
    if (m3) {
      const s = m3[1];
      return this.setHex(parseInt(s[0] + s[0] + s[1] + s[1] + s[2] + s[2], 16));
    }
    return this;
  }
  copy(c) { this.r = c.r; this.g = c.g; this.b = c.b; return this; }
  clone() { return new Color(this.r, this.g, this.b); }
  multiplyScalar(s) { this.r *= s; this.g *= s; this.b *= s; return this; }
  getHex() {
    return (Math.round(this.r * 255) << 16) ^ (Math.round(this.g * 255) << 8) ^ Math.round(this.b * 255);
  }
}

// ---- src/math/Vector2.js ----
/**
 * Vector2 — 2D vector, API mirrors three.js.
 */
class Vector2 {
  constructor(x = 0, y = 0) {
    this.isVector2 = true;
    this.x = x;
    this.y = y;
  }
  set(x, y) { this.x = x; this.y = y; return this; }
  copy(v) { this.x = v.x; this.y = v.y; return this; }
  clone() { return new Vector2(this.x, this.y); }
  add(v) { this.x += v.x; this.y += v.y; return this; }
  sub(v) { this.x -= v.x; this.y -= v.y; return this; }
  multiplyScalar(s) { this.x *= s; this.y *= s; return this; }
  divideScalar(s) { return this.multiplyScalar(1 / s); }
  dot(v) { return this.x * v.x + this.y * v.y; }
  length() { return Math.sqrt(this.x * this.x + this.y * this.y); }
  normalize() {
    const l = this.length();
    return this.divideScalar(l === 0 ? 1 : l);
  }
  distanceTo(v) {
    const dx = this.x - v.x, dy = this.y - v.y;
    return Math.sqrt(dx * dx + dy * dy);
  }
}

// ---- src/math/Vector3.js ----
/**
 * Vector3 — 3D vector, API mirrors three.js.
 */
class Vector3 {
  constructor(x = 0, y = 0, z = 0) {
    this.isVector3 = true;
    this.x = x;
    this.y = y;
    this.z = z;
  }
  set(x, y, z) { this.x = x; this.y = y; this.z = z; return this; }
  copy(v) { this.x = v.x; this.y = v.y; this.z = v.z; return this; }
  clone() { return new Vector3(this.x, this.y, this.z); }
  add(v) { this.x += v.x; this.y += v.y; this.z += v.z; return this; }
  addVectors(a, b) { this.x = a.x + b.x; this.y = a.y + b.y; this.z = a.z + b.z; return this; }
  sub(v) { this.x -= v.x; this.y -= v.y; this.z -= v.z; return this; }
  subVectors(a, b) { this.x = a.x - b.x; this.y = a.y - b.y; this.z = a.z - b.z; return this; }
  multiplyScalar(s) { this.x *= s; this.y *= s; this.z *= s; return this; }
  divideScalar(s) { return this.multiplyScalar(1 / s); }
  dot(v) { return this.x * v.x + this.y * v.y + this.z * v.z; }
  cross(v) {
    const x = this.x, y = this.y, z = this.z;
    this.x = y * v.z - z * v.y;
    this.y = z * v.x - x * v.z;
    this.z = x * v.y - y * v.x;
    return this;
  }
  crossVectors(a, b) {
    this.x = a.y * b.z - a.z * b.y;
    this.y = a.z * b.x - a.x * b.z;
    this.z = a.x * b.y - a.y * b.x;
    return this;
  }
  length() { return Math.sqrt(this.x * this.x + this.y * this.y + this.z * this.z); }
  lengthSq() { return this.x * this.x + this.y * this.y + this.z * this.z; }
  normalize() {
    const l = this.length();
    return this.divideScalar(l === 0 ? 1 : l);
  }
  distanceTo(v) {
    const dx = this.x - v.x, dy = this.y - v.y, dz = this.z - v.z;
    return Math.sqrt(dx * dx + dy * dy + dz * dz);
  }
  applyQuaternion(q) {
    const x = this.x, y = this.y, z = this.z;
    const qx = q.x, qy = q.y, qz = q.z, qw = q.w;
    // v' = q * v * q^-1, expanded (same formulation as three.js)
    const ix = qw * x + qy * z - qz * y;
    const iy = qw * y + qz * x - qx * z;
    const iz = qw * z + qx * y - qy * x;
    const iw = -qx * x - qy * y - qz * z;
    this.x = ix * qw + iw * -qx + iy * -qz - iz * -qy;
    this.y = iy * qw + iw * -qy + iz * -qx - ix * -qz;
    this.z = iz * qw + iw * -qz + ix * -qy - iy * -qx;
    return this;
  }
  applyMatrix4(m) {
    const e = m.elements;
    const x = this.x, y = this.y, z = this.z;
    const w = 1 / (e[3] * x + e[7] * y + e[11] * z + e[15]);
    this.x = (e[0] * x + e[4] * y + e[8] * z + e[12]) * w;
    this.y = (e[1] * x + e[5] * y + e[9] * z + e[13]) * w;
    this.z = (e[2] * x + e[6] * y + e[10] * z + e[14]) * w;
    return this;
  }
  transformDirection(m) {
    const e = m.elements;
    const x = this.x, y = this.y, z = this.z;
    this.x = e[0] * x + e[4] * y + e[8] * z;
    this.y = e[1] * x + e[5] * y + e[9] * z;
    this.z = e[2] * x + e[6] * y + e[10] * z;
    return this.normalize();
  }
  setFromMatrixPosition(m) {
    const e = m.elements;
    this.x = e[12]; this.y = e[13]; this.z = e[14];
    return this;
  }
}

// ---- src/math/Vector4.js ----
/**
 * Vector4 — 4D vector, API mirrors three.js.
 */
class Vector4 {
  constructor(x = 0, y = 0, z = 0, w = 0) {
    this.isVector4 = true;
    this.x = x;
    this.y = y;
    this.z = z;
    this.w = w;
  }
  set(x, y, z, w) { this.x = x; this.y = y; this.z = z; this.w = w; return this; }
  copy(v) { this.x = v.x; this.y = v.y; this.z = v.z; this.w = v.w; return this; }
  clone() { return new Vector4(this.x, this.y, this.z, this.w); }
  add(v) { this.x += v.x; this.y += v.y; this.z += v.z; this.w += v.w; return this; }
  sub(v) { this.x -= v.x; this.y -= v.y; this.z -= v.z; this.w -= v.w; return this; }
  multiplyScalar(s) { this.x *= s; this.y *= s; this.z *= s; this.w *= s; return this; }
  divideScalar(s) { return this.multiplyScalar(1 / s); }
  dot(v) { return this.x * v.x + this.y * v.y + this.z * v.z + this.w * v.w; }
  length() { return Math.sqrt(this.x * this.x + this.y * this.y + this.z * this.z + this.w * this.w); }
  normalize() {
    const l = this.length();
    return this.divideScalar(l === 0 ? 1 : l);
  }
}

// ---- src/math/Euler.js ----
/**
 * Euler — rotation angles (radians), default order 'XYZ'. API mirrors three.js.
 * x/y/z/order are accessor properties so Object3D can keep the quaternion in sync.
 */
class Euler {
  constructor(x = 0, y = 0, z = 0, order = 'XYZ') {
    this._x = x;
    this._y = y;
    this._z = z;
    this._order = order;
    const self = this;
    Object.defineProperties(this, {
      x: {
        get() { return self._x; },
        set(v) { self._x = v; self._onChangeCallback(); },
        enumerable: true, configurable: true,
      },
      y: {
        get() { return self._y; },
        set(v) { self._y = v; self._onChangeCallback(); },
        enumerable: true, configurable: true,
      },
      z: {
        get() { return self._z; },
        set(v) { self._z = v; self._onChangeCallback(); },
        enumerable: true, configurable: true,
      },
      order: {
        get() { return self._order; },
        set(v) { self._order = v; self._onChangeCallback(); },
        enumerable: true, configurable: true,
      },
    });
  }
  _onChangeCallback() {}
  _onChange(callback) { this._onChangeCallback = callback; return this; }
  set(x, y, z, order) {
    this._x = x;
    this._y = y;
    this._z = z;
    if (order !== undefined) this._order = order;
    this._onChangeCallback();
    return this;
  }
  /** Write fields without firing the change callback (used by sync code). */
  _setSilent(x, y, z, order) {
    this._x = x;
    this._y = y;
    this._z = z;
    if (order !== undefined) this._order = order;
    return this;
  }
  copy(e) {
    this._x = e.x; this._y = e.y; this._z = e.z; this._order = e.order;
    this._onChangeCallback();
    return this;
  }
  clone() { return new Euler(this._x, this._y, this._z, this._order); }
  setFromQuaternion(q) {
    q.toEuler(this);
    return this;
  }
}

// ---- src/math/Quaternion.js ----

/**
 * Quaternion — rotation quaternion. API mirrors three.js.
 * Components are accessor properties so Object3D can keep the Euler in sync.
 * All mutating methods write the backing fields and fire the callback once.
 */
class Quaternion {
  constructor(x = 0, y = 0, z = 0, w = 1) {
    this._x = x;
    this._y = y;
    this._z = z;
    this._w = w;
    const self = this;
    const mk = (field) => ({
      get() { return self[field]; },
      set(v) { self[field] = v; self._onChangeCallback(); },
      enumerable: true, configurable: true,
    });
    Object.defineProperties(this, {
      x: mk('_x'),
      y: mk('_y'),
      z: mk('_z'),
      w: mk('_w'),
    });
  }
  _onChangeCallback() {}
  _onChange(callback) { this._onChangeCallback = callback; return this; }

  set(x, y, z, w) {
    this._x = x; this._y = y; this._z = z; this._w = w;
    this._onChangeCallback();
    return this;
  }
  copy(q) {
    this._x = q.x; this._y = q.y; this._z = q.z; this._w = q.w;
    this._onChangeCallback();
    return this;
  }
  clone() { return new Quaternion(this._x, this._y, this._z, this._w); }
  identity() { return this.set(0, 0, 0, 1); }

  setFromEuler(euler) {
    const x = euler.x, y = euler.y, z = euler.z, order = euler.order || 'XYZ';
    const c1 = Math.cos(x / 2), c2 = Math.cos(y / 2), c3 = Math.cos(z / 2);
    const s1 = Math.sin(x / 2), s2 = Math.sin(y / 2), s3 = Math.sin(z / 2);
    switch (order) {
      case 'XYZ':
        this._x = s1 * c2 * c3 + c1 * s2 * s3;
        this._y = c1 * s2 * c3 - s1 * c2 * s3;
        this._z = c1 * c2 * s3 + s1 * s2 * c3;
        this._w = c1 * c2 * c3 - s1 * s2 * s3;
        break;
      case 'YXZ':
        this._x = s1 * c2 * c3 + c1 * s2 * s3;
        this._y = c1 * s2 * c3 - s1 * c2 * s3;
        this._z = c1 * c2 * s3 - s1 * s2 * c3;
        this._w = c1 * c2 * c3 + s1 * s2 * s3;
        break;
      case 'ZXY':
        this._x = s1 * c2 * c3 - c1 * s2 * s3;
        this._y = c1 * s2 * c3 + s1 * c2 * s3;
        this._z = c1 * c2 * s3 + s1 * s2 * c3;
        this._w = c1 * c2 * c3 - s1 * s2 * s3;
        break;
      case 'ZYX':
        this._x = s1 * c2 * c3 - c1 * s2 * s3;
        this._y = c1 * s2 * c3 + s1 * c2 * s3;
        this._z = c1 * c2 * s3 - s1 * s2 * c3;
        this._w = c1 * c2 * c3 + s1 * s2 * s3;
        break;
      case 'YZX':
        this._x = s1 * c2 * c3 + c1 * s2 * s3;
        this._y = c1 * s2 * c3 + s1 * c2 * s3;
        this._z = c1 * c2 * s3 - s1 * s2 * c3;
        this._w = c1 * c2 * c3 - s1 * s2 * s3;
        break;
      case 'XZY':
        this._x = s1 * c2 * c3 - c1 * s2 * s3;
        this._y = c1 * s2 * c3 - s1 * c2 * s3;
        this._z = c1 * c2 * s3 + s1 * s2 * c3;
        this._w = c1 * c2 * c3 + s1 * s2 * s3;
        break;
      default:
        throw new Error('Quaternion.setFromEuler: unknown order ' + order);
    }
    this._onChangeCallback();
    return this;
  }

  setFromAxisAngle(axis, angle) {
    const half = angle / 2;
    const s = Math.sin(half);
    this._x = axis.x * s;
    this._y = axis.y * s;
    this._z = axis.z * s;
    this._w = Math.cos(half);
    this._onChangeCallback();
    return this;
  }

  setFromRotationMatrix(m) {
    const te = m.elements;
    const m11 = te[0], m12 = te[4], m13 = te[8];
    const m21 = te[1], m22 = te[5], m23 = te[9];
    const m31 = te[2], m32 = te[6], m33 = te[10];
    const trace = m11 + m22 + m33;
    if (trace > 0) {
      const s = 0.5 / Math.sqrt(trace + 1.0);
      this._w = 0.25 / s;
      this._x = (m32 - m23) * s;
      this._y = (m13 - m31) * s;
      this._z = (m21 - m12) * s;
    } else if (m11 > m22 && m11 > m33) {
      const s = 2.0 * Math.sqrt(1.0 + m11 - m22 - m33);
      this._w = (m32 - m23) / s;
      this._x = 0.25 * s;
      this._y = (m12 + m21) / s;
      this._z = (m13 + m31) / s;
    } else if (m22 > m33) {
      const s = 2.0 * Math.sqrt(1.0 + m22 - m11 - m33);
      this._w = (m13 - m31) / s;
      this._x = (m12 + m21) / s;
      this._y = 0.25 * s;
      this._z = (m23 + m32) / s;
    } else {
      const s = 2.0 * Math.sqrt(1.0 + m33 - m11 - m22);
      this._w = (m21 - m12) / s;
      this._x = (m13 + m31) / s;
      this._y = (m23 + m32) / s;
      this._z = 0.25 * s;
    }
    this._onChangeCallback();
    return this;
  }

  multiply(q) { return this.multiplyQuaternions(this, q); }
  premultiply(q) { return this.multiplyQuaternions(q, this); }
  multiplyQuaternions(a, b) {
    const qax = a.x, qay = a.y, qaz = a.z, qaw = a.w;
    const qbx = b.x, qby = b.y, qbz = b.z, qbw = b.w;
    this._x = qax * qbw + qaw * qbx + qay * qbz - qaz * qby;
    this._y = qay * qbw + qaw * qby + qaz * qbx - qax * qbz;
    this._z = qaz * qbw + qaw * qbz + qax * qby - qay * qbx;
    this._w = qaw * qbw - qax * qbx - qay * qby - qaz * qbz;
    this._onChangeCallback();
    return this;
  }

  length() { return Math.sqrt(this._x ** 2 + this._y ** 2 + this._z ** 2 + this._w ** 2); }
  normalize() {
    let l = this.length();
    if (l === 0) { this._x = 0; this._y = 0; this._z = 0; this._w = 1; }
    else {
      l = 1 / l;
      this._x *= l; this._y *= l; this._z *= l; this._w *= l;
    }
    this._onChangeCallback();
    return this;
  }
  conjugate() {
    this._x *= -1; this._y *= -1; this._z *= -1;
    this._onChangeCallback();
    return this;
  }
  invert() {
    // conjugate + normalize, without double-firing
    this._x *= -1; this._y *= -1; this._z *= -1;
    let l = this.length();
    if (l === 0) { this._x = 0; this._y = 0; this._z = 0; this._w = 1; }
    else {
      l = 1 / l;
      this._x *= l; this._y *= l; this._z *= l; this._w *= l;
    }
    this._onChangeCallback();
    return this;
  }
  dot(q) { return this._x * q.x + this._y * q.y + this._z * q.z + this._w * q.w; }

  slerp(qb, t) {
    if (t === 0) return this;
    if (t === 1) return this.copy(qb);
    const x = this._x, y = this._y, z = this._z, w = this._w;
    let cosHalfTheta = w * qb.w + x * qb.x + y * qb.y + z * qb.z;
    let bx = qb.x, by = qb.y, bz = qb.z, bw = qb.w;
    if (cosHalfTheta < 0) {
      bx = -bx; by = -by; bz = -bz; bw = -bw;
      cosHalfTheta = -cosHalfTheta;
    }
    if (cosHalfTheta >= 1.0) return this;
    const sinHalfTheta = Math.sqrt(1.0 - cosHalfTheta * cosHalfTheta);
    if (Math.abs(sinHalfTheta) < 0.001) {
      this._w = w * 0.5 + bw * 0.5;
      this._x = x * 0.5 + bx * 0.5;
      this._y = y * 0.5 + by * 0.5;
      this._z = z * 0.5 + bz * 0.5;
      this._onChangeCallback();
      return this;
    }
    const halfTheta = Math.acos(cosHalfTheta);
    const ratioA = Math.sin((1 - t) * halfTheta) / sinHalfTheta;
    const ratioB = Math.sin(t * halfTheta) / sinHalfTheta;
    this._w = w * ratioA + bw * ratioB;
    this._x = x * ratioA + bx * ratioB;
    this._y = y * ratioA + by * ratioB;
    this._z = z * ratioA + bz * ratioB;
    this._onChangeCallback();
    return this;
  }

  /** Convert to an XYZ Euler. Writes silently when the target supports it. */
  toEuler(optionalResult) {
    const e = optionalResult || new Euler();
    const x = this._x, y = this._y, z = this._z, w = this._w;
    const m13 = 2 * (x * z + y * w);
    const ey = Math.asin(Math.max(-1, Math.min(1, m13)));
    let ex, ez;
    if (Math.abs(m13) < 0.9999999) {
      ex = Math.atan2(-2 * (y * z - x * w), 1 - 2 * (x * x + y * y));
      ez = Math.atan2(-2 * (x * y - z * w), 1 - 2 * (y * y + z * z));
    } else {
      ex = Math.atan2(2 * (x * y + z * w), 1 - 2 * (x * x + z * z));
      ez = 0;
    }
    if (typeof e._setSilent === 'function') e._setSilent(ex, ey, ez, 'XYZ');
    else e.set(ex, ey, ez, 'XYZ');
    return e;
  }
}

// ---- src/math/Matrix4.js ----
/**
 * Matrix4 — 4x4 matrix, column-major `elements` (like three.js / WebGL).
 */
class Matrix4 {
  constructor() {
    this.elements = new Float32Array([
      1, 0, 0, 0,
      0, 1, 0, 0,
      0, 0, 1, 0,
      0, 0, 0, 1,
    ]);
  }
  identity() {
    const te = this.elements;
    te.fill(0);
    te[0] = 1; te[5] = 1; te[10] = 1; te[15] = 1;
    return this;
  }
  copy(m) { this.elements.set(m.elements); return this; }
  clone() { const m = new Matrix4(); m.elements.set(this.elements); return m; }
  set(n11, n12, n13, n14, n21, n22, n23, n24, n31, n32, n33, n34, n41, n42, n43, n44) {
    const te = this.elements;
    te[0] = n11; te[4] = n12; te[8] = n13; te[12] = n14;
    te[1] = n21; te[5] = n22; te[9] = n23; te[13] = n24;
    te[2] = n31; te[6] = n32; te[10] = n33; te[14] = n34;
    te[3] = n41; te[7] = n42; te[11] = n43; te[15] = n44;
    return this;
  }
  multiply(m) { return this.multiplyMatrices(this, m); }
  premultiply(m) { return this.multiplyMatrices(m, this); }
  multiplyMatrices(a, b) {
    const ae = a.elements, be = b.elements, te = this.elements;
    const a11 = ae[0], a12 = ae[4], a13 = ae[8], a14 = ae[12];
    const a21 = ae[1], a22 = ae[5], a23 = ae[9], a24 = ae[13];
    const a31 = ae[2], a32 = ae[6], a33 = ae[10], a34 = ae[14];
    const a41 = ae[3], a42 = ae[7], a43 = ae[11], a44 = ae[15];
    const b11 = be[0], b12 = be[4], b13 = be[8], b14 = be[12];
    const b21 = be[1], b22 = be[5], b23 = be[9], b24 = be[13];
    const b31 = be[2], b32 = be[6], b33 = be[10], b34 = be[14];
    const b41 = be[3], b42 = be[7], b43 = be[11], b44 = be[15];
    te[0] = a11 * b11 + a12 * b21 + a13 * b31 + a14 * b41;
    te[4] = a11 * b12 + a12 * b22 + a13 * b32 + a14 * b42;
    te[8] = a11 * b13 + a12 * b23 + a13 * b33 + a14 * b43;
    te[12] = a11 * b14 + a12 * b24 + a13 * b34 + a14 * b44;
    te[1] = a21 * b11 + a22 * b21 + a23 * b31 + a24 * b41;
    te[5] = a21 * b12 + a22 * b22 + a23 * b32 + a24 * b42;
    te[9] = a21 * b13 + a22 * b23 + a23 * b33 + a24 * b43;
    te[13] = a21 * b14 + a22 * b24 + a23 * b34 + a24 * b44;
    te[2] = a31 * b11 + a32 * b21 + a33 * b31 + a34 * b41;
    te[6] = a31 * b12 + a32 * b22 + a33 * b32 + a34 * b42;
    te[10] = a31 * b13 + a32 * b23 + a33 * b33 + a34 * b43;
    te[14] = a31 * b14 + a32 * b24 + a33 * b34 + a34 * b44;
    te[3] = a41 * b11 + a42 * b21 + a43 * b31 + a44 * b41;
    te[7] = a41 * b12 + a42 * b22 + a43 * b32 + a44 * b42;
    te[11] = a41 * b13 + a42 * b23 + a43 * b33 + a44 * b43;
    te[15] = a41 * b14 + a42 * b24 + a43 * b34 + a44 * b44;
    return this;
  }
  determinant() {
    const te = this.elements;
    const n11 = te[0], n12 = te[4], n13 = te[8], n14 = te[12];
    const n21 = te[1], n22 = te[5], n23 = te[9], n24 = te[13];
    const n31 = te[2], n32 = te[6], n33 = te[10], n34 = te[14];
    const n41 = te[3], n42 = te[7], n43 = te[11], n44 = te[15];
    return (
      n41 * (+n14 * n23 * n32 - n13 * n24 * n32 - n14 * n22 * n33 + n12 * n24 * n33 + n13 * n22 * n34 - n12 * n23 * n34) +
      n42 * (+n11 * n23 * n34 - n11 * n24 * n33 + n14 * n21 * n33 - n13 * n21 * n34 + n13 * n24 * n31 - n14 * n23 * n31) +
      n43 * (+n11 * n24 * n32 - n11 * n22 * n34 - n14 * n21 * n32 + n12 * n21 * n34 + n14 * n22 * n31 - n12 * n24 * n31) +
      n44 * (-n13 * n22 * n31 - n11 * n23 * n32 + n11 * n22 * n33 + n13 * n21 * n32 - n12 * n21 * n33 + n12 * n23 * n31)
    );
  }
  transpose() {
    const te = this.elements;
    let tmp;
    tmp = te[1]; te[1] = te[4]; te[4] = tmp;
    tmp = te[2]; te[2] = te[8]; te[8] = tmp;
    tmp = te[6]; te[6] = te[9]; te[9] = tmp;
    tmp = te[3]; te[3] = te[12]; te[12] = tmp;
    tmp = te[7]; te[7] = te[13]; te[13] = tmp;
    tmp = te[11]; te[11] = te[14]; te[14] = tmp;
    return this;
  }
  invert() {
    const te = this.elements;
    const n11 = te[0], n21 = te[1], n31 = te[2], n41 = te[3];
    const n12 = te[4], n22 = te[5], n32 = te[6], n42 = te[7];
    const n13 = te[8], n23 = te[9], n33 = te[10], n43 = te[11];
    const n14 = te[12], n24 = te[13], n34 = te[14], n44 = te[15];
    const t11 = n23 * n34 * n42 - n24 * n33 * n42 + n24 * n32 * n43 - n22 * n34 * n43 - n23 * n32 * n44 + n22 * n33 * n44;
    const t12 = n14 * n33 * n42 - n13 * n34 * n42 - n14 * n32 * n43 + n12 * n34 * n43 + n13 * n32 * n44 - n12 * n33 * n44;
    const t13 = n13 * n24 * n42 - n14 * n23 * n42 + n14 * n22 * n43 - n12 * n24 * n43 - n13 * n22 * n44 + n12 * n23 * n44;
    const t14 = n14 * n23 * n32 - n13 * n24 * n32 - n14 * n22 * n33 + n12 * n24 * n33 + n13 * n22 * n34 - n12 * n23 * n34;
    const det = n11 * t11 + n21 * t12 + n31 * t13 + n41 * t14;
    if (det === 0) return this.set(0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0);
    const detInv = 1 / det;
    te[0] = t11 * detInv;
    te[1] = (n24 * n33 * n41 - n23 * n34 * n41 - n24 * n31 * n43 + n21 * n34 * n43 + n23 * n31 * n44 - n21 * n33 * n44) * detInv;
    te[2] = (n22 * n34 * n41 - n24 * n32 * n41 + n24 * n31 * n42 - n21 * n34 * n42 - n22 * n31 * n44 + n21 * n32 * n44) * detInv;
    te[3] = (n23 * n32 * n41 - n22 * n33 * n41 - n23 * n31 * n42 + n21 * n33 * n42 + n22 * n31 * n43 - n21 * n32 * n43) * detInv;
    te[4] = t12 * detInv;
    te[5] = (n13 * n34 * n41 - n14 * n33 * n41 + n14 * n31 * n43 - n11 * n34 * n43 - n13 * n31 * n44 + n11 * n33 * n44) * detInv;
    te[6] = (n14 * n32 * n41 - n12 * n34 * n41 - n14 * n31 * n42 + n11 * n34 * n42 + n12 * n31 * n44 - n11 * n32 * n44) * detInv;
    te[7] = (n12 * n33 * n41 - n13 * n32 * n41 + n13 * n31 * n42 - n11 * n33 * n42 - n12 * n31 * n43 + n11 * n32 * n43) * detInv;
    te[8] = t13 * detInv;
    te[9] = (n14 * n23 * n41 - n13 * n24 * n41 - n14 * n21 * n43 + n11 * n24 * n43 + n13 * n21 * n44 - n11 * n23 * n44) * detInv;
    te[10] = (n12 * n24 * n41 - n14 * n22 * n41 + n14 * n21 * n42 - n11 * n24 * n42 - n12 * n21 * n44 + n11 * n22 * n44) * detInv;
    te[11] = (n13 * n22 * n41 - n12 * n23 * n41 - n13 * n21 * n42 + n11 * n23 * n42 + n12 * n21 * n43 - n11 * n22 * n43) * detInv;
    te[12] = t14 * detInv;
    te[13] = (n13 * n24 * n31 - n14 * n23 * n31 + n14 * n21 * n33 - n11 * n24 * n33 - n13 * n21 * n34 + n11 * n23 * n34) * detInv;
    te[14] = (n14 * n22 * n31 - n12 * n24 * n31 - n14 * n21 * n32 + n11 * n24 * n32 + n12 * n21 * n34 - n11 * n22 * n34) * detInv;
    te[15] = (n12 * n23 * n31 - n13 * n22 * n31 + n13 * n21 * n32 - n11 * n23 * n32 - n12 * n21 * n33 + n11 * n22 * n33) * detInv;
    return this;
  }
  makeTranslation(x, y, z) {
    return this.set(
      1, 0, 0, x,
      0, 1, 0, y,
      0, 0, 1, z,
      0, 0, 0, 1
    );
  }
  makeScale(x, y, z) {
    return this.set(
      x, 0, 0, 0,
      0, y, 0, 0,
      0, 0, z, 0,
      0, 0, 0, 1
    );
  }
  makeRotationFromQuaternion(q) {
    const x = q.x, y = q.y, z = q.z, w = q.w;
    const x2 = x + x, y2 = y + y, z2 = z + z;
    const xx = x * x2, xy = x * y2, xz = x * z2;
    const yy = y * y2, yz = y * z2, zz = z * z2;
    const wx = w * x2, wy = w * y2, wz = w * z2;
    return this.set(
      1 - (yy + zz), xy - wz, xz + wy, 0,
      xy + wz, 1 - (xx + zz), yz - wx, 0,
      xz - wy, yz + wx, 1 - (xx + yy), 0,
      0, 0, 0, 1
    );
  }
  makeRotationFromEuler(euler) {
    const te = this.elements;
    const x = euler.x, y = euler.y, z = euler.z;
    const a = Math.cos(x), b = Math.sin(x);
    const c = Math.cos(y), d = Math.sin(y);
    const e = Math.cos(z), f = Math.sin(z);
    // Only XYZ (default) is fully expanded; other orders fall back via quaternion.
    if ((euler.order || 'XYZ') !== 'XYZ') {
      const { Quaternion } = Matrix4._quatModule || {};
      if (Quaternion) {
        const q = new Quaternion().setFromEuler(euler);
        return this.makeRotationFromQuaternion(q);
      }
    }
    const ae = a * e, af = a * f, be = b * e, bf = b * f;
    te[0] = c * e; te[4] = -c * f; te[8] = d;
    te[1] = af + be * d; te[5] = ae - bf * d; te[9] = -b * c;
    te[2] = bf - ae * d; te[6] = be + af * d; te[10] = a * c;
    te[3] = 0; te[7] = 0; te[11] = 0;
    te[12] = 0; te[13] = 0; te[14] = 0; te[15] = 1;
    return this;
  }
  compose(position, quaternion, scale) {
    this.makeRotationFromQuaternion(quaternion);
    const te = this.elements;
    te[0] *= scale.x; te[1] *= scale.x; te[2] *= scale.x;
    te[4] *= scale.y; te[5] *= scale.y; te[6] *= scale.y;
    te[8] *= scale.z; te[9] *= scale.z; te[10] *= scale.z;
    te[12] = position.x; te[13] = position.y; te[14] = position.z;
    return this;
  }
  decompose(position, quaternion, scale) {
    const te = this.elements;
    let sx = Math.sqrt(te[0] * te[0] + te[1] * te[1] + te[2] * te[2]);
    const sy = Math.sqrt(te[4] * te[4] + te[5] * te[5] + te[6] * te[6]);
    const sz = Math.sqrt(te[8] * te[8] + te[9] * te[9] + te[10] * te[10]);
    if (this.determinant() < 0) sx = -sx;
    position.x = te[12]; position.y = te[13]; position.z = te[14];
    const m = new Matrix4();
    const me = m.elements;
    const invSX = 1 / sx, invSY = 1 / sy, invSZ = 1 / sz;
    me[0] = te[0] * invSX; me[1] = te[1] * invSX; me[2] = te[2] * invSX;
    me[4] = te[4] * invSY; me[5] = te[5] * invSY; me[6] = te[6] * invSY;
    me[8] = te[8] * invSZ; me[9] = te[9] * invSZ; me[10] = te[10] * invSZ;
    me[15] = 1;
    quaternion.setFromRotationMatrix(m);
    scale.x = sx; scale.y = sy; scale.z = sz;
    return this;
  }
  lookAt(eye, target, up) {
    const z = _v1.subVectors(eye, target);
    if (z.lengthSq() === 0) z.z = 1;
    z.normalize();
    const x = _v2.crossVectors(up, z);
    if (x.lengthSq() === 0) {
      if (Math.abs(up.z) === 1) z.x += 0.0001;
      else z.z += 0.0001;
      z.normalize();
      x.crossVectors(up, z);
    }
    x.normalize();
    const y = _v3.crossVectors(z, x);
    return this.set(
      x.x, y.x, z.x, eye.x,
      x.y, y.y, z.y, eye.y,
      x.z, y.z, z.z, eye.z,
      0, 0, 0, 1
    );
  }
  makePerspective(left, right, top, bottom, near, far) {
    const te = this.elements;
    const x = (2 * near) / (right - left);
    const y = (2 * near) / (top - bottom);
    const a = (right + left) / (right - left);
    const b = (top + bottom) / (top - bottom);
    const c = -(far + near) / (far - near);
    const d = (-2 * far * near) / (far - near);
    te[0] = x; te[4] = 0; te[8] = a; te[12] = 0;
    te[1] = 0; te[5] = y; te[9] = b; te[13] = 0;
    te[2] = 0; te[6] = 0; te[10] = c; te[14] = d;
    te[3] = 0; te[7] = 0; te[11] = -1; te[15] = 0;
    return this;
  }
  makeOrthographic(left, right, top, bottom, near, far) {
    const te = this.elements;
    const w = 1 / (right - left);
    const h = 1 / (top - bottom);
    const p = 1 / (far - near);
    const x = (right + left) * w;
    const y = (top + bottom) * h;
    const z = (far + near) * p;
    te[0] = 2 * w; te[4] = 0; te[8] = 0; te[12] = -x;
    te[1] = 0; te[5] = 2 * h; te[9] = 0; te[13] = -y;
    te[2] = 0; te[6] = 0; te[10] = -2 * p; te[14] = -z;
    te[3] = 0; te[7] = 0; te[11] = 0; te[15] = 1;
    return this;
  }
}

// (Quaternion/Vector3 are imported normally; neither defines Matrix4,
// so there is no cycle at class-definition time.)
Matrix4._quatModule = { Quaternion };
const _v1 = new Vector3();
const _v2 = new Vector3();
const _v3 = new Vector3();

// ---- src/core/Object3D.js ----

const _euler = new Euler();

/**
 * Object3D — base node of the scene graph. API mirrors three.js.
 * `rotation` (Euler) and `quaternion` are kept in sync both ways.
 */
class Object3D {
  constructor() {
    this.isObject3D = true;
    this.name = '';
    this.parent = null;
    this.children = [];
    this.up = new Vector3(0, 1, 0);
    this.position = new Vector3();
    this.scale = new Vector3(1, 1, 1);
    this.quaternion = new Quaternion();
    this.rotation = new Euler();
    // Keep Euler and Quaternion in sync both ways (same pattern as three.js).
    this.rotation._onChange(() => {
      this.quaternion.setFromEuler(this.rotation);
    });
    this.quaternion._onChange(() => {
      this.quaternion.toEuler(this.rotation);
    });
    this.matrix = new Matrix4();
    this.matrixWorld = new Matrix4();
    this.matrixAutoUpdate = true;
    this.matrixWorldNeedsUpdate = false;
    this.visible = true;
    this.userData = {};
  }

  add(...objects) {
    for (const object of objects) {
      if (object === this) continue;
      if (object && object.isObject3D) {
        if (object.parent !== null) object.parent.remove(object);
        object.parent = this;
        this.children.push(object);
      }
    }
    return this;
  }
  remove(...objects) {
    for (const object of objects) {
      const index = this.children.indexOf(object);
      if (index !== -1) {
        object.parent = null;
        this.children.splice(index, 1);
      }
    }
    return this;
  }
  clear() {
    for (let i = this.children.length - 1; i >= 0; i--) this.remove(this.children[i]);
    return this;
  }
  traverse(callback) {
    callback(this);
    for (const child of this.children) child.traverse(callback);
  }
  getObjectByName(name) {
    if (this.name === name) return this;
    for (const child of this.children) {
      const found = child.getObjectByName(name);
      if (found) return found;
    }
    return undefined;
  }

  lookAt(x, y, z) {
    const target = (x && x.isVector3) ? x : new Vector3(x, y, z);
    this.updateWorldMatrix(true, false);
    const position = new Vector3().setFromMatrixPosition(this.matrixWorld);
    const m = new Matrix4();
    // Cameras and lights look down their -Z axis; other objects face +Z.
    // (Same convention split as three.js.)
    if (this.isCamera || this.isLight) {
      m.lookAt(position, target, this.up);
    } else {
      m.lookAt(target, position, this.up);
    }
    this.quaternion.setFromRotationMatrix(m);
    if (this.parent) {
      const pp = new Vector3();
      const pq = new Quaternion();
      const ps = new Vector3();
      this.parent.matrixWorld.decompose(pp, pq, ps);
      this.quaternion.premultiply(pq.invert());
    }
  }

  updateMatrix() {
    this.matrix.compose(this.position, this.quaternion, this.scale);
    this.matrixWorldNeedsUpdate = true;
  }
  updateMatrixWorld(force = false) {
    if (this.matrixAutoUpdate) this.updateMatrix();
    if (this.matrixWorldNeedsUpdate || force) {
      if (this.parent === null) {
        this.matrixWorld.copy(this.matrix);
      } else {
        this.matrixWorld.multiplyMatrices(this.parent.matrixWorld, this.matrix);
      }
      force = true;
      this.matrixWorldNeedsUpdate = false;
    }
    for (const child of this.children) child.updateMatrixWorld(force);
  }
  getWorldPosition(target = new Vector3()) {
    this.updateWorldMatrix(true, false);
    return target.setFromMatrixPosition(this.matrixWorld);
  }
  updateWorldMatrix(updateParents, updateChildren) {
    if (updateParents && this.parent) this.parent.updateWorldMatrix(true, false);
    if (this.matrixAutoUpdate) this.updateMatrix();
    if (this.parent === null) this.matrixWorld.copy(this.matrix);
    else this.matrixWorld.multiplyMatrices(this.parent.matrixWorld, this.matrix);
    if (updateChildren) {
      for (const child of this.children) child.updateWorldMatrix(false, true);
    }
  }
}

Object3D.DEFAULT_UP = new Vector3(0, 1, 0);
const _worldPos = new Vector3();
void _euler;

// ---- src/core/Scene.js ----

/**
 * Scene — root of the graph. `background` is a Color (v1: solid color only).
 */
class Scene extends Object3D {
  constructor() {
    super();
    this.isScene = true;
    this.background = null; // Color | null
    this.fog = null; // reserved, not implemented in v1
  }
  setBackground(color) {
    this.background = color instanceof Color ? color : new Color(color);
    return this;
  }
}

// ---- src/core/Camera.js ----

/**
 * Camera base class.
 */
class Camera extends Object3D {
  constructor() {
    super();
    this.isCamera = true;
    this.projectionMatrix = new Matrix4();
    this.matrixWorldInverse = new Matrix4();
  }
  updateMatrixWorld(force) {
    super.updateMatrixWorld(force);
    this.matrixWorldInverse.copy(this.matrixWorld).invert();
  }
}

class PerspectiveCamera extends Camera {
  constructor(fov = 50, aspect = 1, near = 0.1, far = 2000) {
    super();
    this.isPerspectiveCamera = true;
    this.fov = fov;
    this.aspect = aspect;
    this.near = near;
    this.far = far;
    this.updateProjectionMatrix();
  }
  updateProjectionMatrix() {
    const near = this.near, far = this.far;
    const top = near * Math.tan((Math.PI / 180) * 0.5 * this.fov);
    const height = 2 * top;
    const width = this.aspect * height;
    const left = -0.5 * width;
    this.projectionMatrix.makePerspective(left, left + width, top, top - height, near, far);
    return this;
  }
}

class OrthographicCamera extends Camera {
  constructor(left = -1, right = 1, top = 1, bottom = -1, near = 0.1, far = 2000) {
    super();
    this.isOrthographicCamera = true;
    this.left = left;
    this.right = right;
    this.top = top;
    this.bottom = bottom;
    this.near = near;
    this.far = far;
    this.updateProjectionMatrix();
  }
  updateProjectionMatrix() {
    this.projectionMatrix.makeOrthographic(
      this.left, this.right, this.top, this.bottom, this.near, this.far
    );
    return this;
  }
}

// ---- src/core/BufferGeometry.js ----
/**
 * BufferAttribute — a typed array + itemSize, like three.js.
 */
class BufferAttribute {
  constructor(array, itemSize) {
    this.isBufferAttribute = true;
    this.array = array instanceof Float32Array || array instanceof Uint16Array || array instanceof Uint32Array
      ? array
      : new Float32Array(array);
    this.itemSize = itemSize;
    this.count = this.array.length / itemSize;
    this.needsUpdate = false;
    this._glBuffer = null;
    this._glVersion = -1;
    this.version = 0;
  }
  setUsage() { return this; }
}

class BufferGeometry {
  constructor() {
    this.isBufferGeometry = true;
    this.attributes = {};
    this.index = null;
    this.boundingSphere = null;
    this._version = 0;
  }
  setAttribute(name, attribute) {
    this.attributes[name] = attribute instanceof BufferAttribute
      ? attribute
      : new BufferAttribute(attribute, name === 'uv' || name === 'uv2' ? 2 : 3);
    return this;
  }
  getAttribute(name) { return this.attributes[name]; }
  setIndex(index) {
    if (index === null) { this.index = null; return this; }
    if (index instanceof BufferAttribute) { this.index = index; return this; }
    const arr = (Array.isArray(index) ? index : index.array ? Array.from(index.array) : index);
    const max = this.attributes.position ? this.attributes.position.count : 0;
    const Typed = max > 65535 ? Uint32Array : Uint16Array;
    this.index = new BufferAttribute(new Typed(arr), 1);
    return this;
  }
  getIndex() { return this.index; }
  computeVertexNormals() {
    const position = this.attributes.position;
    if (!position) return this;
    const p = position.array;
    let normals = this.attributes.normal;
    if (!normals || normals.array.length !== p.length) {
      normals = new BufferAttribute(new Float32Array(p.length), 3);
      this.attributes.normal = normals;
    } else {
      normals.array.fill(0);
    }
    const n = normals.array;
    const vA = [0, 0, 0], vB = [0, 0, 0], vC = [0, 0, 0];
    const cb = [0, 0, 0], ab = [0, 0, 0];
    const addFace = (a, b, c) => {
      vA[0] = p[a * 3]; vA[1] = p[a * 3 + 1]; vA[2] = p[a * 3 + 2];
      vB[0] = p[b * 3]; vB[1] = p[b * 3 + 1]; vB[2] = p[b * 3 + 2];
      vC[0] = p[c * 3]; vC[1] = p[c * 3 + 1]; vC[2] = p[c * 3 + 2];
      cb[0] = vC[0] - vB[0]; cb[1] = vC[1] - vB[1]; cb[2] = vC[2] - vB[2];
      ab[0] = vA[0] - vB[0]; ab[1] = vA[1] - vB[1]; ab[2] = vA[2] - vB[2];
      const nx = cb[1] * ab[2] - cb[2] * ab[1];
      const ny = cb[2] * ab[0] - cb[0] * ab[2];
      const nz = cb[0] * ab[1] - cb[1] * ab[0];
      n[a * 3] += nx; n[a * 3 + 1] += ny; n[a * 3 + 2] += nz;
      n[b * 3] += nx; n[b * 3 + 1] += ny; n[b * 3 + 2] += nz;
      n[c * 3] += nx; n[c * 3 + 1] += ny; n[c * 3 + 2] += nz;
    };
    if (this.index) {
      const idx = this.index.array;
      for (let i = 0; i < idx.length; i += 3) addFace(idx[i], idx[i + 1], idx[i + 2]);
    } else {
      for (let i = 0; i < position.count; i += 3) addFace(i, i + 1, i + 2);
    }
    for (let i = 0; i < n.length; i += 3) {
      const l = Math.sqrt(n[i] * n[i] + n[i + 1] * n[i + 1] + n[i + 2] * n[i + 2]) || 1;
      n[i] /= l; n[i + 1] /= l; n[i + 2] /= l;
    }
    normals.needsUpdate = true;
    return this;
  }
  computeBoundingSphere() {
    const position = this.attributes.position;
    if (!position) { this.boundingSphere = { center: [0, 0, 0], radius: 0 }; return this; }
    const p = position.array;
    let minX = Infinity, minY = Infinity, minZ = Infinity;
    let maxX = -Infinity, maxY = -Infinity, maxZ = -Infinity;
    for (let i = 0; i < p.length; i += 3) {
      if (p[i] < minX) minX = p[i]; if (p[i] > maxX) maxX = p[i];
      if (p[i + 1] < minY) minY = p[i + 1]; if (p[i + 1] > maxY) maxY = p[i + 1];
      if (p[i + 2] < minZ) minZ = p[i + 2]; if (p[i + 2] > maxZ) maxZ = p[i + 2];
    }
    const cx = (minX + maxX) / 2, cy = (minY + maxY) / 2, cz = (minZ + maxZ) / 2;
    let radius = 0;
    for (let i = 0; i < p.length; i += 3) {
      const dx = p[i] - cx, dy = p[i + 1] - cy, dz = p[i + 2] - cz;
      radius = Math.max(radius, Math.sqrt(dx * dx + dy * dy + dz * dz));
    }
    this.boundingSphere = { center: [cx, cy, cz], radius };
    return this;
  }
  dispose(gl) {
    if (!gl) return;
    for (const name of Object.keys(this.attributes)) {
      const attr = this.attributes[name];
      if (attr._glBuffer) { gl.deleteBuffer(attr._glBuffer); attr._glBuffer = null; }
    }
    if (this.index && this.index._glBuffer) {
      gl.deleteBuffer(this.index._glBuffer);
      this.index._glBuffer = null;
    }
  }
}

// ---- src/core/Texture.js ----
/**
 * Texture — wraps an image source (HTMLImageElement, HTMLCanvasElement,
 * ImageBitmap, or {width,height,data} raw RGBA). API mirrors three.js (subset).
 */
class Texture {
  constructor(image = null) {
    this.isTexture = true;
    this.image = image;
    this.wrapS = Texture.ClampToEdgeWrapping;
    this.wrapT = Texture.ClampToEdgeWrapping;
    this.magFilter = Texture.LinearFilter;
    this.minFilter = Texture.LinearMipmapLinearFilter;
    this.generateMipmaps = true;
    this.flipY = true;
    this.needsUpdate = true;
    this.version = 0;
    // internal GL state, managed by the renderer
    this._glTexture = null;
    this._glVersion = -1;
  }
  setImage(image) {
    this.image = image;
    this.needsUpdate = true;
    return this;
  }
  dispose(gl) {
    if (gl && this._glTexture) gl.deleteTexture(this._glTexture);
    this._glTexture = null;
    this._glVersion = -1;
  }
}

Texture.ClampToEdgeWrapping = 1000;
Texture.RepeatWrapping = 1001;
Texture.MirroredRepeatWrapping = 1002;
Texture.NearestFilter = 1003;
Texture.LinearFilter = 1004;
Texture.LinearMipmapLinearFilter = 1005;
Texture.NearestMipmapLinearFilter = 1006;

// ---- src/core/Mesh.js ----

/**
 * Mesh — an Object3D with geometry + material. Group is a plain Object3D alias.
 */
class Mesh extends Object3D {
  constructor(geometry, material) {
    super();
    this.isMesh = true;
    this.geometry = geometry;
    this.material = material !== undefined ? material : new MeshBasicMaterial({ color: 0xffffff });
  }
}

class Group extends Object3D {
  constructor() {
    super();
    this.isGroup = true;
  }
}

// ---- src/materials/Material.js ----

/**
 * Material base + MeshBasicMaterial + MeshPhongMaterial.
 * Parameter handling mirrors three.js: constructor takes a params object.
 */
class Material {
  constructor() {
    this.isMaterial = true;
    this.name = '';
    this.visible = true;
    this.side = Material.FrontSide;
    this.transparent = false;
    this.opacity = 1;
    this.depthWrite = true;
    this.depthTest = true;
    this.wireframe = false;
  }
  setValues(values) {
    if (!values) return this;
    for (const key of Object.keys(values)) {
      const newValue = values[key];
      if (newValue === undefined) continue;
      const current = this[key];
      if (current && current.isColor) current.set(newValue);
      else this[key] = newValue;
    }
    return this;
  }
  dispose() {}
}

Material.FrontSide = 0;
Material.BackSide = 1;
Material.DoubleSide = 2;

class MeshBasicMaterial extends Material {
  constructor(parameters) {
    super();
    this.isMeshBasicMaterial = true;
    this.type = 'MeshBasicMaterial';
    this.color = new Color(0xffffff);
    this.map = null;
    this.setValues(parameters);
  }
}

/**
 * MeshPhongMaterial — per-fragment Blinn-Phong lighting.
 * Supports ambient + directional + point lights, `map`, specular, shininess.
 */
class MeshPhongMaterial extends Material {
  constructor(parameters) {
    super();
    this.isMeshPhongMaterial = true;
    this.type = 'MeshPhongMaterial';
    this.color = new Color(0xffffff);
    this.specular = new Color(0x111111);
    this.shininess = 30;
    this.emissive = new Color(0x000000);
    this.map = null;
    this.flatShading = false;
    this.setValues(parameters);
  }
}

// ---- src/lights/Light.js ----

/**
 * Lights. Mirrors three.js class names and semantics (subset).
 */
class Light extends Object3D {
  constructor(color = 0xffffff, intensity = 1) {
    super();
    this.isLight = true;
    this.color = new Color(color);
    this.intensity = intensity;
  }
}

class AmbientLight extends Light {
  constructor(color = 0xffffff, intensity = 1) {
    super(color, intensity);
    this.isAmbientLight = true;
  }
}

class DirectionalLight extends Light {
  constructor(color = 0xffffff, intensity = 1) {
    super(color, intensity);
    this.isDirectionalLight = true;
    this.position.set(0, 1, 0);
    this.target = new Object3D();
  }
}

class PointLight extends Light {
  constructor(color = 0xffffff, intensity = 1, distance = 0, decay = 2) {
    super(color, intensity);
    this.isPointLight = true;
    this.distance = distance; // 0 = infinite
    this.decay = decay;
  }
}

// ---- src/geometries/BoxGeometry.js ----

/**
 * BoxGeometry — axis-aligned box centered at origin. Mirrors three.js.
 */
class BoxGeometry extends BufferGeometry {
  constructor(width = 1, height = 1, depth = 1, widthSegments = 1, heightSegments = 1, depthSegments = 1) {
    super();
    this.type = 'BoxGeometry';
    const positions = [];
    const normals = [];
    const uvs = [];
    const indices = [];
    let vertexOffset = 0;

    const buildPlane = (u, v, w, udir, vdir, width, height, depth, gridX, gridY) => {
      const segmentWidth = width / gridX;
      const segmentHeight = height / gridY;
      const widthHalf = width / 2;
      const heightHalf = height / 2;
      const depthHalf = depth / 2;
      const gridX1 = gridX + 1;
      const gridY1 = gridY + 1;
      const vec = [0, 0, 0];
      for (let iy = 0; iy < gridY1; iy++) {
        const y = iy * segmentHeight - heightHalf;
        for (let ix = 0; ix < gridX1; ix++) {
          const x = ix * segmentWidth - widthHalf;
          vec[u] = x * udir;
          vec[v] = y * vdir;
          vec[w] = depthHalf;
          positions.push(vec[0], vec[1], vec[2]);
          vec[u] = 0; vec[v] = 0; vec[w] = 0;
          vec[w] = depth > 0 ? 1 : -1;
          normals.push(vec[0], vec[1], vec[2]);
          uvs.push(ix / gridX, 1 - iy / gridY);
        }
      }
      for (let iy = 0; iy < gridY; iy++) {
        for (let ix = 0; ix < gridX; ix++) {
          const a = vertexOffset + ix + gridX1 * iy;
          const b = vertexOffset + ix + gridX1 * (iy + 1);
          const c = vertexOffset + (ix + 1) + gridX1 * (iy + 1);
          const d = vertexOffset + (ix + 1) + gridX1 * iy;
          indices.push(a, b, d, b, c, d);
        }
      }
      vertexOffset += gridX1 * gridY1;
    };

    // px, nx, py, ny, pz, nz (same layout as three.js)
    buildPlane(2, 1, 0, -1, -1, depth, height, width, depthSegments, heightSegments); // px
    buildPlane(2, 1, 0, 1, -1, depth, height, -width, depthSegments, heightSegments); // nx
    buildPlane(0, 2, 1, 1, 1, width, depth, height, widthSegments, depthSegments); // py
    buildPlane(0, 2, 1, 1, -1, width, depth, -height, widthSegments, depthSegments); // ny
    buildPlane(0, 1, 2, 1, -1, width, height, depth, widthSegments, heightSegments); // pz
    buildPlane(0, 1, 2, -1, -1, width, height, -depth, widthSegments, heightSegments); // nz

    this.setAttribute('position', new BufferAttribute(new Float32Array(positions), 3));
    this.setAttribute('normal', new BufferAttribute(new Float32Array(normals), 3));
    this.setAttribute('uv', new BufferAttribute(new Float32Array(uvs), 2));
    this.setIndex(indices);
  }
}

// ---- src/geometries/PlaneGeometry.js ----

/**
 * PlaneGeometry — XY plane facing +Z, centered at origin. Mirrors three.js.
 */
class PlaneGeometry extends BufferGeometry {
  constructor(width = 1, height = 1, widthSegments = 1, heightSegments = 1) {
    super();
    this.type = 'PlaneGeometry';
    const widthHalf = width / 2;
    const heightHalf = height / 2;
    const gridX = Math.floor(widthSegments);
    const gridY = Math.floor(heightSegments);
    const gridX1 = gridX + 1;
    const gridY1 = gridY + 1;
    const segmentWidth = width / gridX;
    const segmentHeight = height / gridY;
    const positions = [];
    const normals = [];
    const uvs = [];
    const indices = [];
    for (let iy = 0; iy < gridY1; iy++) {
      const y = iy * segmentHeight - heightHalf;
      for (let ix = 0; ix < gridX1; ix++) {
        const x = ix * segmentWidth - widthHalf;
        positions.push(x, -y, 0);
        normals.push(0, 0, 1);
        uvs.push(ix / gridX, 1 - iy / gridY);
      }
    }
    for (let iy = 0; iy < gridY; iy++) {
      for (let ix = 0; ix < gridX; ix++) {
        const a = ix + gridX1 * iy;
        const b = ix + gridX1 * (iy + 1);
        const c = ix + 1 + gridX1 * (iy + 1);
        const d = ix + 1 + gridX1 * iy;
        indices.push(a, b, d, b, c, d);
      }
    }
    this.setAttribute('position', new BufferAttribute(new Float32Array(positions), 3));
    this.setAttribute('normal', new BufferAttribute(new Float32Array(normals), 3));
    this.setAttribute('uv', new BufferAttribute(new Float32Array(uvs), 2));
    this.setIndex(indices);
  }
}

// ---- src/geometries/SphereGeometry.js ----

/**
 * SphereGeometry — UV sphere. Mirrors three.js.
 */
class SphereGeometry extends BufferGeometry {
  constructor(radius = 1, widthSegments = 32, heightSegments = 16, phiStart = 0, phiLength = Math.PI * 2, thetaStart = 0, thetaLength = Math.PI) {
    super();
    this.type = 'SphereGeometry';
    widthSegments = Math.max(3, Math.floor(widthSegments));
    heightSegments = Math.max(2, Math.floor(heightSegments));
    const positions = [];
    const normals = [];
    const uvs = [];
    const indices = [];
    const grid = [];
    for (let iy = 0; iy <= heightSegments; iy++) {
      const row = [];
      const v = iy / heightSegments;
      let uOffset = 0;
      if (iy === 0 && thetaStart === 0) uOffset = 0.5 / widthSegments;
      else if (iy === heightSegments && thetaLength === Math.PI) uOffset = -0.5 / widthSegments;
      for (let ix = 0; ix <= widthSegments; ix++) {
        const u = ix / widthSegments;
        const phi = phiStart + u * phiLength;
        const theta = thetaStart + v * thetaLength;
        const x = -radius * Math.cos(phi) * Math.sin(theta);
        const y = radius * Math.cos(theta);
        const z = radius * Math.sin(phi) * Math.sin(theta);
        positions.push(x, y, z);
        const len = Math.sqrt(x * x + y * y + z * z) || 1;
        normals.push(x / len, y / len, z / len);
        uvs.push(u + uOffset, 1 - v);
        row.push(positions.length / 3 - 1);
      }
      grid.push(row);
    }
    for (let iy = 0; iy < heightSegments; iy++) {
      for (let ix = 0; ix < widthSegments; ix++) {
        const a = grid[iy][ix + 1];
        const b = grid[iy][ix];
        const c = grid[iy + 1][ix];
        const d = grid[iy + 1][ix + 1];
        if (iy !== 0 || thetaStart > 0) indices.push(a, b, d);
        if (iy !== heightSegments - 1 || thetaLength < Math.PI) indices.push(b, c, d);
      }
    }
    this.setAttribute('position', new BufferAttribute(new Float32Array(positions), 3));
    this.setAttribute('normal', new BufferAttribute(new Float32Array(normals), 3));
    this.setAttribute('uv', new BufferAttribute(new Float32Array(uvs), 2));
    this.setIndex(indices);
  }
}

// ---- src/core/WebGLRenderer.js ----

/**
 * WebGLRenderer — WebGL2 renderer.
 *
 * Architecture note: all GL work goes through this class's small internal
 * surface (program cache, attribute upload, texture upload, draw). A future
 * WebGPU backend can implement the same surface behind `render()`.
 *
 * Color pipeline (v1, deliberately simple): material colors and textures are
 * treated as sRGB, linearized in-shader/on-upload, lighting happens in
 * linear space, and the final color is gamma-encoded (pow 1/2.2).
 * No tone mapping in v1.
 */

const MAX_DIR_LIGHTS = 4;
const MAX_POINT_LIGHTS = 4;

const VERT = /* glsl */ `
attribute vec3 position;
attribute vec3 normal;
attribute vec2 uv;
uniform mat4 modelMatrix;
uniform mat4 modelViewMatrix;
uniform mat4 projectionMatrix;
uniform mat4 viewMatrix;
varying vec3 vNormal;
varying vec3 vWorldPos;
varying vec2 vUv;
void main() {
  vUv = uv;
  vec4 worldPos = modelMatrix * vec4(position, 1.0);
  vWorldPos = worldPos.xyz;
  vNormal = mat3(modelMatrix) * normal;
  gl_Position = projectionMatrix * viewMatrix * worldPos;
}
`;

const FRAG_BASIC = /* glsl */ `
precision highp float;
varying vec3 vNormal;
varying vec3 vWorldPos;
varying vec2 vUv;
uniform vec3 color;
uniform float opacity;
#ifdef USE_MAP
uniform sampler2D map;
#endif
void main() {
  vec4 c = vec4(color, opacity);
  #ifdef USE_MAP
    vec4 tex = texture2D(map, vUv);
    c.rgb *= pow(tex.rgb, vec3(2.2));
    c.a *= tex.a;
  #endif
  gl_FragColor = vec4(pow(c.rgb, vec3(1.0 / 2.2)), c.a);
}
`;

const FRAG_PHONG = /* glsl */ `
precision highp float;
varying vec3 vNormal;
varying vec3 vWorldPos;
varying vec2 vUv;
uniform vec3 color;
uniform vec3 specular;
uniform vec3 emissive;
uniform float shininess;
uniform float opacity;
uniform vec3 cameraPosition;
#ifdef USE_MAP
uniform sampler2D map;
#endif
uniform vec3 ambientColor;
uniform int numDirLights;
uniform vec3 dirLightColor[${MAX_DIR_LIGHTS}];
uniform vec3 dirLightDirection[${MAX_DIR_LIGHTS}];
uniform int numPointLights;
uniform vec3 pointLightColor[${MAX_POINT_LIGHTS}];
uniform vec3 pointLightPosition[${MAX_POINT_LIGHTS}];
uniform vec2 pointLightParams[${MAX_POINT_LIGHTS}]; // x: distance (0=inf), y: decay
void main() {
  vec3 albedo = color;
  float alpha = opacity;
  #ifdef USE_MAP
    vec4 tex = texture2D(map, vUv);
    albedo *= pow(tex.rgb, vec3(2.2));
    alpha *= tex.a;
  #endif
  #ifdef FLAT_SHADED
    vec3 N = normalize(cross(dFdx(vWorldPos), dFdy(vWorldPos)));
  #else
    vec3 N = normalize(vNormal);
  #endif
  vec3 V = normalize(cameraPosition - vWorldPos);
  vec3 result = emissive + ambientColor * albedo;
  for (int i = 0; i < ${MAX_DIR_LIGHTS}; i++) {
    if (i >= numDirLights) break;
    vec3 L = normalize(dirLightDirection[i]);
    float diff = max(dot(N, L), 0.0);
    vec3 H = normalize(L + V);
    float spec = pow(max(dot(N, H), 0.0), shininess);
    result += dirLightColor[i] * (diff * albedo + spec * specular);
  }
  for (int i = 0; i < ${MAX_POINT_LIGHTS}; i++) {
    if (i >= numPointLights) break;
    vec3 toLight = pointLightPosition[i] - vWorldPos;
    float dist = length(toLight);
    vec3 L = toLight / max(dist, 0.0001);
    float attenuation = 1.0;
    float cutoff = pointLightParams[i].x;
    float decay = pointLightParams[i].y;
    if (cutoff > 0.0) {
      float f = clamp(1.0 - dist / cutoff, 0.0, 1.0);
      attenuation *= f * f;
    }
    if (decay != 0.0) attenuation *= 1.0 / pow(max(dist, 0.001), decay);
    float diff = max(dot(N, L), 0.0);
    vec3 H = normalize(L + V);
    float spec = pow(max(dot(N, H), 0.0), shininess);
    result += attenuation * pointLightColor[i] * (diff * albedo + spec * specular);
  }
  gl_FragColor = vec4(pow(result, vec3(1.0 / 2.2)), alpha);
}
`;

function srgbToLinear(c) {
  return Math.pow(c, 2.2);
}

class WebGLRenderer {
  constructor(parameters = {}) {
    this.isWebGLRenderer = true;
    this.domElement = parameters.canvas || document.createElement('canvas');
    const gl = this.domElement.getContext('webgl2', {
      antialias: parameters.antialias !== false,
      alpha: parameters.alpha === true,
    });
    if (!gl) throw new Error('Yousj3D: WebGL2 is not available in this browser.');
    this._gl = gl;
    this._pixelRatio = parameters.pixelRatio || (typeof window !== 'undefined' ? window.devicePixelRatio || 1 : 1);
    this._width = 0;
    this._height = 0;
    this._programs = new Map();
    this._vaos = new WeakMap();
    this._animationLoop = null;
    this._rafId = null;
    this._clockStart = performance.now();
    this.info = { render: { calls: 0, triangles: 0 } };
    this.autoClear = true;
    gl.enable(gl.DEPTH_TEST);
    gl.depthFunc(gl.LEQUAL);
  }

  getContext() { return this._gl; }
  setPixelRatio(ratio) {
    this._pixelRatio = ratio;
    if (this._width) this.setSize(this._width, this._height, false);
  }
  getPixelRatio() { return this._pixelRatio; }
  setSize(width, height, updateStyle = true) {
    this._width = width;
    this._height = height;
    const canvas = this.domElement;
    canvas.width = Math.floor(width * this._pixelRatio);
    canvas.height = Math.floor(height * this._pixelRatio);
    if (updateStyle !== false) {
      canvas.style.width = width + 'px';
      canvas.style.height = height + 'px';
    }
    this._gl.viewport(0, 0, canvas.width, canvas.height);
  }
  getSize(target = {}) {
    target.width = this._width;
    target.height = this._height;
    return target;
  }
  setAnimationLoop(callback) {
    this._animationLoop = callback;
    if (this._rafId !== null) cancelAnimationFrame(this._rafId);
    this._rafId = null;
    if (callback) {
      const loop = () => {
        this._rafId = requestAnimationFrame(loop);
        callback(performance.now() - this._clockStart);
      };
      this._rafId = requestAnimationFrame(loop);
    }
  }
  dispose() {
    this.setAnimationLoop(null);
    for (const entry of this._programs.values()) this._gl.deleteProgram(entry.program);
    this._programs.clear();
  }

  // ---- programs ----
  _getProgram(material) {
    const isPhong = !!material.isMeshPhongMaterial;
    const useMap = !!material.map;
    const flat = isPhong && !!material.flatShading;
    const key = (isPhong ? 'phong' : 'basic') + (useMap ? '+map' : '') + (flat ? '+flat' : '');
    let entry = this._programs.get(key);
    if (entry) return entry;
    const gl = this._gl;
    const fragSrc = (isPhong ? FRAG_PHONG : FRAG_BASIC);
    const defines = (useMap ? '#define USE_MAP\n' : '') + (flat ? '#define FLAT_SHADED\n' : '');
    const vs = this._compile(gl.VERTEX_SHADER, VERT);
    const fs = this._compile(gl.FRAGMENT_SHADER, defines + fragSrc);
    const program = gl.createProgram();
    gl.attachShader(program, vs);
    gl.attachShader(program, fs);
    gl.bindAttribLocation(program, 0, 'position');
    gl.bindAttribLocation(program, 1, 'normal');
    gl.bindAttribLocation(program, 2, 'uv');
    gl.linkProgram(program);
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
      throw new Error('Yousj3D: program link failed: ' + gl.getProgramInfoLog(program));
    }
    gl.deleteShader(vs);
    gl.deleteShader(fs);
    const uniforms = {};
    const count = gl.getProgramParameter(program, gl.ACTIVE_UNIFORMS);
    for (let i = 0; i < count; i++) {
      const info = gl.getActiveUniform(program, i);
      if (info.name.endsWith('[0]')) {
        const base = info.name.slice(0, -3);
        for (let j = 0; j < info.size; j++) {
          uniforms[base + '[' + j + ']'] = gl.getUniformLocation(program, base + '[' + j + ']');
        }
        uniforms[base] = uniforms[base + '[0]'];
      } else {
        uniforms[info.name] = gl.getUniformLocation(program, info.name);
      }
    }
    entry = { program, uniforms };
    this._programs.set(key, entry);
    return entry;
  }
  _compile(type, src) {
    const gl = this._gl;
    const shader = gl.createShader(type);
    gl.shaderSource(shader, src);
    gl.compileShader(shader);
    if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
      const log = gl.getShaderInfoLog(shader);
      gl.deleteShader(shader);
      throw new Error('Yousj3D: shader compile failed: ' + log);
    }
    return shader;
  }

  // ---- geometry / textures ----
  _uploadAttribute(attr, target) {
    const gl = this._gl;
    if (!attr._glBuffer) {
      attr._glBuffer = gl.createBuffer();
      attr._glVersion = -1;
    }
    gl.bindBuffer(target, attr._glBuffer);
    if (attr._glVersion !== attr.version || attr.needsUpdate) {
      gl.bufferData(target, attr.array, gl.STATIC_DRAW);
      attr._glVersion = attr.version;
      attr.needsUpdate = false;
    }
  }
  _getVAO(geometry) {
    let vao = this._vaos.get(geometry);
    if (vao) {
      // Refresh any attribute data flagged as changed since VAO creation.
      const gl = this._gl;
      for (const name of Object.keys(geometry.attributes)) {
        const attr = geometry.attributes[name];
        if (attr.needsUpdate && attr._glBuffer) {
          gl.bindBuffer(gl.ARRAY_BUFFER, attr._glBuffer);
          gl.bufferData(gl.ARRAY_BUFFER, attr.array, gl.STATIC_DRAW);
          attr.needsUpdate = false;
          attr._glVersion = attr.version;
        }
      }
      return vao;
    }
    const gl = this._gl;
    vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    const attrs = [
      ['position', 3],
      ['normal', 3],
      ['uv', 2],
    ];
    attrs.forEach(([name], loc) => {
      const attr = geometry.attributes[name];
      if (attr) {
        this._uploadAttribute(attr, gl.ARRAY_BUFFER);
        gl.enableVertexAttribArray(loc);
        gl.vertexAttribPointer(loc, attr.itemSize, gl.FLOAT, false, 0, 0);
      }
    });
    if (geometry.index) this._uploadAttribute(geometry.index, gl.ELEMENT_ARRAY_BUFFER);
    gl.bindVertexArray(null);
    this._vaos.set(geometry, vao);
    geometry._vao = vao;
    return vao;
  }
  _uploadTexture(texture) {
    const gl = this._gl;
    if (texture._glTexture && texture._glVersion === texture.version && !texture.needsUpdate) {
      return texture._glTexture;
    }
    if (!texture._glTexture) texture._glTexture = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, texture._glTexture);
    const img = texture.image;
    const wrap = (w) => (w === Texture.RepeatWrapping ? gl.REPEAT : w === Texture.MirroredRepeatWrapping ? gl.MIRRORED_REPEAT : gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, wrap(texture.wrapS));
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, wrap(texture.wrapT));
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, texture.magFilter === Texture.NearestFilter ? gl.NEAREST : gl.LINEAR);
    const minFilter = texture.minFilter === Texture.NearestFilter ? gl.NEAREST
      : texture.minFilter === Texture.NearestMipmapLinearFilter ? gl.NEAREST_MIPMAP_LINEAR
      : gl.LINEAR_MIPMAP_LINEAR;
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, minFilter);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, texture.flipY ? 1 : 0);
    if (img && img.data && img.width !== undefined) {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, img.width, img.height, 0, gl.RGBA, gl.UNSIGNED_BYTE, img.data);
    } else if (img) {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, img);
    } else {
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, 1, 1, 0, gl.RGBA, gl.UNSIGNED_BYTE, new Uint8Array([255, 255, 255, 255]));
    }
    if (texture.generateMipmaps && minFilter !== gl.NEAREST && minFilter !== gl.LINEAR) {
      gl.generateMipmap(gl.TEXTURE_2D);
    }
    texture._glVersion = texture.version;
    texture.needsUpdate = false;
    return texture._glTexture;
  }

  // ---- render ----
  render(scene, camera) {
    const gl = this._gl;
    if (!camera || !camera.isCamera) throw new Error('Yousj3D: render() needs a Camera.');
    scene.updateMatrixWorld();
    camera.updateMatrixWorld();

    // clear
    if (scene.background && scene.background.isColor) {
      gl.clearColor(scene.background.r, scene.background.g, scene.background.b, 1);
    } else {
      gl.clearColor(0, 0, 0, 1);
    }
    if (this.autoClear) gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);

    // collect lights + meshes
    const ambient = new Float32Array(3);
    const dirLights = [];
    const pointLights = [];
    const opaque = [];
    const transparent = [];
    const camPos = new Vector3().setFromMatrixPosition(camera.matrixWorld);
    scene.traverse((obj) => {
      if (obj.isAmbientLight) {
        ambient[0] += srgbToLinear(obj.color.r) * obj.intensity;
        ambient[1] += srgbToLinear(obj.color.g) * obj.intensity;
        ambient[2] += srgbToLinear(obj.color.b) * obj.intensity;
      } else if (obj.isDirectionalLight) {
        if (dirLights.length < MAX_DIR_LIGHTS) {
          obj.target.updateWorldMatrix(true, false);
          const lp = new Vector3().setFromMatrixPosition(obj.matrixWorld);
          const tp = new Vector3().setFromMatrixPosition(obj.target.matrixWorld);
          const dir = lp.sub(tp).normalize(); // direction TOWARD the light
          dirLights.push({ color: obj.color, intensity: obj.intensity, dir });
        }
      } else if (obj.isPointLight) {
        if (pointLights.length < MAX_POINT_LIGHTS) {
          const pos = new Vector3().setFromMatrixPosition(obj.matrixWorld);
          pointLights.push({ color: obj.color, intensity: obj.intensity, position: pos, distance: obj.distance, decay: obj.decay });
        }
      } else if (obj.isMesh && obj.visible && obj.geometry && obj.material && obj.material.visible) {
        if (obj.material.transparent) transparent.push(obj);
        else opaque.push(obj);
      }
    });
    transparent.sort((a, b) => {
      const pa = new Vector3().setFromMatrixPosition(a.matrixWorld);
      const pb = new Vector3().setFromMatrixPosition(b.matrixWorld);
      return pb.distanceTo(camPos) - pa.distanceTo(camPos);
    });

    this.info.render.calls = 0;
    this.info.render.triangles = 0;

    const viewMatrix = camera.matrixWorldInverse;
    for (const mesh of [...opaque, ...transparent]) {
      this._renderMesh(mesh, scene, camera, viewMatrix, camPos, ambient, dirLights, pointLights);
    }
  }

  _renderMesh(mesh, scene, camera, viewMatrix, camPos, ambient, dirLights, pointLights) {
    const gl = this._gl;
    const material = mesh.material;
    const geometry = mesh.geometry;
    if (material.isMeshPhongMaterial && !geometry.attributes.normal) {
      geometry.computeVertexNormals();
    }
    const { program, uniforms } = this._getProgram(material);
    gl.useProgram(program);
    gl.bindVertexArray(this._getVAO(geometry));

    // matrices
    const modelView = new Matrix4().multiplyMatrices(viewMatrix, mesh.matrixWorld);
    if (uniforms.modelMatrix) gl.uniformMatrix4fv(uniforms.modelMatrix, false, mesh.matrixWorld.elements);
    if (uniforms.modelViewMatrix) gl.uniformMatrix4fv(uniforms.modelViewMatrix, false, modelView.elements);
    if (uniforms.projectionMatrix) gl.uniformMatrix4fv(uniforms.projectionMatrix, false, camera.projectionMatrix.elements);
    if (uniforms.viewMatrix) gl.uniformMatrix4fv(uniforms.viewMatrix, false, viewMatrix.elements);
    if (uniforms.cameraPosition) gl.uniform3f(uniforms.cameraPosition, camPos.x, camPos.y, camPos.z);

    // material uniforms
    const c = material.color;
    if (uniforms.color) gl.uniform3f(uniforms.color, srgbToLinear(c.r), srgbToLinear(c.g), srgbToLinear(c.b));
    if (uniforms.opacity) gl.uniform1f(uniforms.opacity, material.opacity);
    if (material.map) {
      gl.activeTexture(gl.TEXTURE0);
      gl.bindTexture(gl.TEXTURE_2D, this._uploadTexture(material.map));
      if (uniforms.map) gl.uniform1i(uniforms.map, 0);
    }

    if (material.isMeshPhongMaterial) {
      const s = material.specular;
      if (uniforms.specular) gl.uniform3f(uniforms.specular, srgbToLinear(s.r), srgbToLinear(s.g), srgbToLinear(s.b));
      const e = material.emissive;
      if (uniforms.emissive) gl.uniform3f(uniforms.emissive, srgbToLinear(e.r), srgbToLinear(e.g), srgbToLinear(e.b));
      if (uniforms.shininess) gl.uniform1f(uniforms.shininess, material.shininess);
      if (uniforms.ambientColor) gl.uniform3f(uniforms.ambientColor, ambient[0], ambient[1], ambient[2]);
      if (uniforms.numDirLights) gl.uniform1i(uniforms.numDirLights, dirLights.length);
      dirLights.forEach((light, i) => {
        const colorLoc = uniforms['dirLightColor[' + i + ']'] || uniforms.dirLightColor;
        const dirLoc = uniforms['dirLightDirection[' + i + ']'] || uniforms.dirLightDirection;
        if (colorLoc) gl.uniform3f(colorLoc, srgbToLinear(light.color.r) * light.intensity, srgbToLinear(light.color.g) * light.intensity, srgbToLinear(light.color.b) * light.intensity);
        if (dirLoc) gl.uniform3f(dirLoc, light.dir.x, light.dir.y, light.dir.z);
      });
      if (uniforms.numPointLights) gl.uniform1i(uniforms.numPointLights, pointLights.length);
      pointLights.forEach((light, i) => {
        const colorLoc = uniforms['pointLightColor[' + i + ']'] || uniforms.pointLightColor;
        const posLoc = uniforms['pointLightPosition[' + i + ']'] || uniforms.pointLightPosition;
        const parLoc = uniforms['pointLightParams[' + i + ']'] || uniforms.pointLightParams;
        if (colorLoc) gl.uniform3f(colorLoc, srgbToLinear(light.color.r) * light.intensity, srgbToLinear(light.color.g) * light.intensity, srgbToLinear(light.color.b) * light.intensity);
        if (posLoc) gl.uniform3f(posLoc, light.position.x, light.position.y, light.position.z);
        if (parLoc) gl.uniform2f(parLoc, light.distance, light.decay);
      });
    }

    // state
    if (material.depthTest) gl.enable(gl.DEPTH_TEST);
    else gl.disable(gl.DEPTH_TEST);
    gl.depthMask(material.depthWrite);
    if (material.transparent) {
      gl.enable(gl.BLEND);
      gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    } else {
      gl.disable(gl.BLEND);
    }
    if (material.side === Material.DoubleSide) {
      gl.disable(gl.CULL_FACE);
    } else {
      gl.enable(gl.CULL_FACE);
      gl.cullFace(material.side === Material.BackSide ? gl.FRONT : gl.BACK);
    }

    // draw
    const position = geometry.attributes.position;
    if (material.wireframe) {
      // Cheap wireframe: draw triangles as lines (approximate; v1).
      if (geometry.index) {
        gl.drawElements(gl.LINES, geometry.index.count, geometry.index.array instanceof Uint32Array ? gl.UNSIGNED_INT : gl.UNSIGNED_SHORT, 0);
      } else {
        gl.drawArrays(gl.LINES, 0, position.count);
      }
    } else if (geometry.index) {
      gl.drawElements(gl.TRIANGLES, geometry.index.count, geometry.index.array instanceof Uint32Array ? gl.UNSIGNED_INT : gl.UNSIGNED_SHORT, 0);
      this.info.render.triangles += geometry.index.count / 3;
    } else {
      gl.drawArrays(gl.TRIANGLES, 0, position.count);
      this.info.render.triangles += position.count / 3;
    }
    this.info.render.calls++;
    gl.bindVertexArray(null);
  }
}

// ---- src/loaders/OBJLoader.js ----

/**
 * OBJLoader — minimal Wavefront OBJ parser (v/vt/vn/f, quads fan-triangulated).
 * Returns a Group with one Mesh per `o`/`g` section (single shared material).
 * For GLB, see the roadmap in the README — not in v1.
 */
class OBJLoader {
  constructor() {
    this.material = new MeshPhongMaterial({ color: 0xcccccc });
  }
  setMaterial(material) { this.material = material; return this; }

  parse(text) {
    const group = new Group();
    const positions = [];
    const normals = [];
    const uvs = [];
    let cur = null; // current section accumulator

    const startSection = (name) => {
      cur = { name: name || '', pos: [], norm: [], uv: [], index: [], cache: new Map(), vertCount: 0 };
    };
    startSection('default');

    const resolveIndex = (idx, arr) => {
      const i = parseInt(idx, 10);
      return i < 0 ? arr.length / 3 + i : i - 1; // note: for uv (2-wide) handled separately below
    };
    const resolveIndex2 = (idx, count) => {
      const i = parseInt(idx, 10);
      return i < 0 ? count + i : i - 1;
    };

    const addVertex = (vIdx, tIdx, nIdx) => {
      const key = vIdx + '/' + (tIdx ?? '') + '/' + (nIdx ?? '');
      if (cur.cache.has(key)) return cur.cache.get(key);
      const outIndex = cur.vertCount++;
      cur.pos.push(positions[vIdx * 3], positions[vIdx * 3 + 1], positions[vIdx * 3 + 2]);
      if (tIdx !== undefined && tIdx !== null && uvs.length) {
        cur.uv.push(uvs[tIdx * 2], uvs[tIdx * 2 + 1]);
      } else {
        cur.uv.push(0, 0);
      }
      if (nIdx !== undefined && nIdx !== null && normals.length) {
        cur.norm.push(normals[nIdx * 3], normals[nIdx * 3 + 1], normals[nIdx * 3 + 2]);
      } else {
        cur.norm.push(0, 0, 0);
      }
      cur.cache.set(key, outIndex);
      return outIndex;
    };

    const finishSection = () => {
      if (!cur || cur.index.length === 0) return;
      const geo = new BufferGeometry();
      geo.setAttribute('position', new BufferAttribute(new Float32Array(cur.pos), 3));
      geo.setAttribute('uv', new BufferAttribute(new Float32Array(cur.uv), 2));
      geo.setAttribute('normal', new BufferAttribute(new Float32Array(cur.norm), 3));
      geo.setIndex(cur.index);
      if (!normals.length) geo.computeVertexNormals();
      const mesh = new Mesh(geo, this.material);
      mesh.name = cur.name;
      group.add(mesh);
    };

    const lines = text.split('\n');
    for (let raw of lines) {
      const line = raw.trim();
      if (!line || line.startsWith('#')) continue;
      const parts = line.split(/\s+/);
      const keyword = parts[0];
      if (keyword === 'v') {
        positions.push(parseFloat(parts[1]), parseFloat(parts[2]), parseFloat(parts[3]));
      } else if (keyword === 'vt') {
        uvs.push(parseFloat(parts[1]), parseFloat(parts[2]));
      } else if (keyword === 'vn') {
        normals.push(parseFloat(parts[1]), parseFloat(parts[2]), parseFloat(parts[3]));
      } else if (keyword === 'o' || keyword === 'g') {
        finishSection();
        startSection(parts.slice(1).join(' '));
      } else if (keyword === 'f') {
        const face = [];
        for (let i = 1; i < parts.length; i++) {
          const comps = parts[i].split('/');
          const v = resolveIndex(comps[0], positions);
          const t = comps.length > 1 && comps[1] !== '' ? resolveIndex2(comps[1], uvs.length / 2) : undefined;
          const n = comps.length > 2 && comps[2] !== '' ? resolveIndex2(comps[2], normals.length / 3) : undefined;
          face.push(addVertex(v, t, n));
        }
        for (let i = 1; i < face.length - 1; i++) {
          cur.index.push(face[0], face[i], face[i + 1]);
        }
      }
      void resolveIndex2;
    }
    finishSection();
    if (group.children.length === 1) return group.children[0];
    return group;
  }

  async load(url) {
    const res = await fetch(url);
    if (!res.ok) throw new Error('OBJLoader: failed to fetch ' + url + ' (' + res.status + ')');
    return this.parse(await res.text());
  }
}

// ---- src/loaders/GLTFLoader.js ----

/**
 * GLTFLoader — minimal GLB (binary glTF 2.0) loader for static meshes.
 * Supports: POSITION / NORMAL / TEXCOORD_0, indices, node hierarchy with
 * TRS or matrix transforms, pbrMetallicRoughness base color factor and
 * base color texture (embedded images). Approximates PBR with Phong:
 * roughness -> shininess, metallic is ignored for specular strength.
 * Not supported: skins, animations, morph targets, KHR extensions, .gltf
 * with external resources. Those are roadmap items.
 */

const COMPONENT_TYPES = {
  5120: Int8Array,
  5121: Uint8Array,
  5122: Int16Array,
  5123: Uint16Array,
  5125: Uint32Array,
  5126: Float32Array,
};
const TYPE_SIZES = { SCALAR: 1, VEC2: 2, VEC3: 3, VEC4: 4, MAT2: 4, MAT3: 9, MAT4: 16 };

class GLTFLoader {
  async load(url) {
    const res = await fetch(url);
    if (!res.ok) throw new Error('GLTFLoader: failed to fetch ' + url + ' (' + res.status + ')');
    return this.parse(await res.arrayBuffer());
  }

  async parse(arrayBuffer) {
    const view = new DataView(arrayBuffer);
    const magic = view.getUint32(0, true);
    if (magic !== 0x46546c67) throw new Error('GLTFLoader: not a GLB file (bad magic).');
    const version = view.getUint32(4, true);
    if (version !== 2) throw new Error('GLTFLoader: only glTF 2.0 GLB is supported.');
    let offset = 12;
    let json = null;
    let bin = null;
    const total = view.getUint32(8, true);
    while (offset < total) {
      const chunkLength = view.getUint32(offset, true);
      const chunkType = view.getUint32(offset + 4, true);
      const chunkStart = offset + 8;
      if (chunkType === 0x4e4f534a) {
        const text = new TextDecoder().decode(new Uint8Array(arrayBuffer, chunkStart, chunkLength));
        json = JSON.parse(text);
      } else if (chunkType === 0x004e4942) {
        bin = new Uint8Array(arrayBuffer, chunkStart, chunkLength);
      }
      offset = chunkStart + chunkLength;
    }
    if (!json) throw new Error('GLTFLoader: GLB has no JSON chunk.');
    if (!bin && (json.buffers && json.buffers.length)) {
      throw new Error('GLTFLoader: GLB has no BIN chunk (external buffers unsupported).');
    }

    const readAccessor = (accessorIndex) => {
      const accessor = json.accessors[accessorIndex];
      const bufferView = json.bufferViews[accessor.bufferView || 0];
      const TypedArray = COMPONENT_TYPES[accessor.componentType];
      const itemSize = TYPE_SIZES[accessor.type];
      const elementBytes = TypedArray.BYTES_PER_ELEMENT * itemSize;
      const stride = bufferView.byteStride || elementBytes;
      const start = (bufferView.byteOffset || 0) + (accessor.byteOffset || 0);
      if (stride === elementBytes) {
        return new TypedArray(bin.buffer, bin.byteOffset + start, accessor.count * itemSize);
      }
      // De-interleave
      const out = new TypedArray(accessor.count * itemSize);
      const src = new TypedArray(bin.buffer, bin.byteOffset + start, 1);
      void src;
      const raw = new DataView(bin.buffer, bin.byteOffset + start);
      const getter = {
        5120: 'getInt8', 5121: 'getUint8', 5122: 'getInt16',
        5123: 'getUint16', 5125: 'getUint32', 5126: 'getFloat32',
      }[accessor.componentType];
      const bpe = TypedArray.BYTES_PER_ELEMENT;
      for (let i = 0; i < accessor.count; i++) {
        for (let j = 0; j < itemSize; j++) {
          out[i * itemSize + j] = raw[getter](i * stride + j * bpe, true);
        }
      }
      return out;
    };

    // Textures (embedded images only)
    const textures = [];
    if (json.textures) {
      for (const texDef of json.textures) {
        const imageDef = json.images[texDef.source];
        const bv = json.bufferViews[imageDef.bufferView];
        const start = bv.byteOffset || 0;
        const blob = new Blob([bin.subarray(start, start + bv.byteLength)], { type: imageDef.mimeType || 'image/png' });
        const url = URL.createObjectURL(blob);
        const img = new Image();
        await new Promise((resolve, reject) => {
          img.onload = resolve;
          img.onerror = reject;
          img.src = url;
        });
        const tex = new Texture(img);
        textures.push(tex);
      }
    }

    // Materials
    const materials = (json.materials || []).map((matDef) => {
      const pbr = matDef.pbrMetallicRoughness || {};
      const factor = pbr.baseColorFactor || [1, 1, 1, 1];
      const mat = new MeshPhongMaterial({
        color: new Color(factor[0], factor[1], factor[2]),
        opacity: factor[3],
        transparent: factor[3] < 1 || matDef.alphaMode === 'BLEND',
        shininess: (1 - (pbr.roughnessFactor !== undefined ? pbr.roughnessFactor : 0.8)) * 120 + 5,
        specular: new Color(0.25, 0.25, 0.25),
      });
      if (matDef.emissiveFactor) {
        mat.emissive = new Color(matDef.emissiveFactor[0], matDef.emissiveFactor[1], matDef.emissiveFactor[2]);
      }
      if (pbr.baseColorTexture) {
        mat.map = textures[pbr.baseColorTexture.index] || null;
      }
      mat.side = matDef.doubleSided ? 2 : 0;
      return mat;
    });
    const defaultMaterial = new MeshPhongMaterial({ color: 0xcccccc });

    // Meshes
    const meshes = (json.meshes || []).map((meshDef) => {
      const meshGroup = new Group();
      meshGroup.name = meshDef.name || '';
      for (const prim of meshDef.primitives) {
        const geo = new BufferGeometry();
        const attrs = prim.attributes;
        if (attrs.POSITION !== undefined) {
          geo.setAttribute('position', new BufferAttribute(asFloat(readAccessor(attrs.POSITION)), 3));
        }
        if (attrs.NORMAL !== undefined) {
          geo.setAttribute('normal', new BufferAttribute(asFloat(readAccessor(attrs.NORMAL)), 3));
        }
        if (attrs.TEXCOORD_0 !== undefined) {
          geo.setAttribute('uv', new BufferAttribute(asFloat(readAccessor(attrs.TEXCOORD_0)), 2));
        }
        if (prim.indices !== undefined) {
          const idx = readAccessor(prim.indices);
          geo.setIndex(new BufferAttribute(idx instanceof Uint32Array ? idx : new (idx.BYTES_PER_ELEMENT === 4 ? Uint32Array : Uint16Array)(idx), 1));
        }
        if (!geo.attributes.normal) geo.computeVertexNormals();
        const mesh = new Mesh(geo, prim.material !== undefined ? materials[prim.material] : defaultMaterial);
        meshGroup.add(mesh);
      }
      return meshGroup.children.length === 1 ? meshGroup.children[0] : meshGroup;
    });

    // Nodes
    const buildNode = (nodeIndex) => {
      const def = json.nodes[nodeIndex];
      const obj = def.mesh !== undefined ? meshes[def.mesh] : new Group();
      obj.name = def.name || obj.name || '';
      if (def.matrix) {
        const m = new Matrix4();
        m.elements.set(def.matrix);
        const pos = new Vector3();
        const quat = new Quaternion();
        const scale = new Vector3();
        m.decompose(pos, quat, scale);
        obj.position.copy(pos);
        obj.quaternion.copy(quat);
        obj.scale.copy(scale);
      } else {
        if (def.translation) obj.position.set(def.translation[0], def.translation[1], def.translation[2]);
        if (def.rotation) obj.quaternion.set(def.rotation[0], def.rotation[1], def.rotation[2], def.rotation[3]);
        if (def.scale) obj.scale.set(def.scale[0], def.scale[1], def.scale[2]);
      }
      if (def.children) {
        for (const childIndex of def.children) obj.add(buildNode(childIndex));
      }
      return obj;
    };

    const sceneIndex = json.scene || 0;
    const sceneDef = (json.scenes || [])[sceneIndex];
    const root = new Group();
    root.name = sceneDef && sceneDef.name ? sceneDef.name : 'glTF';
    if (sceneDef && sceneDef.nodes) {
      for (const nodeIndex of sceneDef.nodes) root.add(buildNode(nodeIndex));
    }
    return root;
  }
}

function asFloat(arr) {
  return arr instanceof Float32Array ? arr : new Float32Array(arr);
}

const REVISION = '0.1.0';
export { Vector2, Vector3, Vector4, Matrix4, Quaternion, Euler, Color, Object3D, Scene, Camera, PerspectiveCamera, OrthographicCamera, BufferGeometry, BufferAttribute, Texture, Mesh, Group, WebGLRenderer, BoxGeometry, PlaneGeometry, SphereGeometry, Material, MeshBasicMaterial, MeshPhongMaterial, Light, AmbientLight, DirectionalLight, PointLight, OBJLoader, GLTFLoader, REVISION };
