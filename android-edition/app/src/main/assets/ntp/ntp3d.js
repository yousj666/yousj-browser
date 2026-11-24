/* Yousj 起始页活背景 —— 由 Yousj 3D（自研引擎）驱动
 * 主图平面（封面式铺满 + 鼠标视差）+ 水面倒影平面（采样楼群带、翻转、缓动）
 * 引擎或图片不可用时静默退回 CSS 静态背景，不影响搜索与书签。 */
import * as Y3D from "./yousj3d.module.js";

(function () {
  var canvas = document.getElementById("gl");
  if (!canvas) return;

  var img = new Image();
  img.onload = function () {
    try { start(img); } catch (e) { /* 保持静态背景 */ }
  };
  img.src = "bg.jpg";

  function start(image) {
    var renderer = new Y3D.WebGLRenderer({ canvas: canvas, antialias: true });
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));

    var scene = new Y3D.Scene();
    var FOV = 40, DIST = 10;
    var camera = new Y3D.PerspectiveCamera(FOV, 1, 0.1, 100);
    camera.position.set(0, 0, DIST);

    var tex = new Y3D.Texture(image);

    /* 主图：单位平面，靠 scale 做 cover 铺满（多留 9% 给视差余量） */
    var main = new Y3D.Mesh(
      new Y3D.PlaneGeometry(1, 1),
      new Y3D.MeshBasicMaterial({ map: tex })
    );

    /* 倒影：同一张图，UV 改采样水线以上的楼群带并翻转 */
    var reflGeo = new Y3D.PlaneGeometry(1, 1);
    var uv = reflGeo.getAttribute("uv");   /* Yousj 3D 的 BufferAttribute 直接读写 array */
    for (var i = 0; i < uv.count; i++) {
      var v = uv.array[i * 2 + 1];         /* 1 = 倒影带上沿（水线），0 = 下沿 */
      uv.array[i * 2 + 1] = 0.40 + (1 - v) * 0.24;
    }
    uv.needsUpdate = true;
    var reflMat = new Y3D.MeshBasicMaterial({ map: tex, transparent: true, opacity: 0.36 });
    var refl = new Y3D.Mesh(reflGeo, reflMat);

    var group = new Y3D.Group();
    group.add(main); group.add(refl);
    scene.add(group);

    var viewW = 0, viewH = 0, planeW = 0, bandH = 0;
    function layout() {
      var w = window.innerWidth, h = window.innerHeight;
      renderer.setSize(w, h, false);
      camera.aspect = w / h;
      camera.updateProjectionMatrix();
      viewH = 2 * DIST * Math.tan((FOV * Math.PI / 180) / 2);
      viewW = viewH * camera.aspect;
      var ia = image.width / image.height;
      planeW = Math.max(viewW, viewH * ia) * 1.09;
      var planeH = planeW / ia;
      if (planeH < viewH * 1.09) { planeH = viewH * 1.09; planeW = planeH * ia; }
      main.scale.set(planeW, planeH, 1);
      main.position.y = (planeH - viewH) * 0.18;   /* 构图略偏向天空，与静态版一致 */
      bandH = viewH * 0.36;
      refl.scale.set(planeW, bandH, 1);
      refl.position.z = 0.02;              /* 略靠前，避免与主图同深 z-fight */
      refl.position.y = -viewH / 2 + bandH / 2 - (planeH - viewH) * 0.02;
    }
    layout();
    window.addEventListener("resize", layout);

    var tx = 0, ty = 0, cx = 0, cy = 0;
    window.addEventListener("mousemove", function (e) {
      tx = (e.clientX / window.innerWidth - 0.5);
      ty = (e.clientY / window.innerHeight - 0.5);
    });

    var reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    var t0 = performance.now();
    function frame(now) {
      var t = (now - t0) / 1000;
      cx += (tx - cx) * 0.045;
      cy += (ty - cy) * 0.045;
      camera.position.x = cx * viewW * 0.022 + Math.sin(t * 0.05) * viewW * 0.0022;
      camera.position.y = -cy * viewH * 0.016 + Math.cos(t * 0.037) * viewH * 0.0016;
      camera.lookAt(0, 0, 0);
      refl.position.x = Math.sin(t * 0.10) * planeW * 0.006;
      reflMat.opacity = 0.34 + Math.sin(t * 0.045) * 0.045;
      renderer.render(scene, camera);
      if (!reduced) requestAnimationFrame(frame);
    }
    requestAnimationFrame(function (now) {
      frame(now || performance.now());
      canvas.classList.add("on");       /* 首帧已出，淡入盖住静态图 */
      window.__yousj3dLive = true;
    });
  }
})();
