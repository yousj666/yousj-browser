package cc.yousj.browser

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.graphics.drawable.GradientDrawable
import android.os.Bundle
import android.view.Gravity
import android.view.KeyEvent
import android.view.View
import android.view.ViewGroup
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputMethodManager
import android.widget.EditText
import android.widget.FrameLayout
import android.widget.ImageButton
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.ProgressBar
import android.widget.Toast
import org.mozilla.geckoview.GeckoResult
import org.mozilla.geckoview.GeckoSession
import org.mozilla.geckoview.WebRequestError
import org.mozilla.geckoview.GeckoView
import java.io.File
import java.net.URLEncoder

class MainActivity : Activity() {

    companion object {
        const val HOME_URL = "resource://android/assets/ntp/ntp.html"
        private const val UBO_ID = "uBlock0@raymondhill.net"
        private const val UBO_ASSET = "resource://android/assets/extensions/ublock-origin.xpi"
    }

    private lateinit var geckoView: GeckoView
    private lateinit var session: GeckoSession
    private lateinit var urlBar: EditText
    private lateinit var progress: ProgressBar
    private var canGoBack = false
    private var canGoForward = false
    private var homeFallbackTried = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val runtime = YousjApp.runtime(application)

        // 内置 uBlock Origin（Firefox 版 .xpi，Gecko 原生 WebExtensions）
        runtime.webExtensionController.ensureBuiltIn(UBO_ASSET, UBO_ID)
            .accept({ /* 已装/已更新 */ }, { /* 装不上也不挡浏览 */ })

        session = GeckoSession()
        session.open(runtime)

        buildUi()
        geckoView.setSession(session)
        wireDelegates()

        val startUrl = intent?.dataString
        if (savedInstanceState == null) {
            session.loadUri(startUrl ?: HOME_URL)
        }
    }

    // ---------- UI（纯代码搭，深色夜城风，与起始页同色系） ----------

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

    private fun barButton(text: String): ImageButton {
        val b = ImageButton(this)
        b.layoutParams = LinearLayout.LayoutParams(dp(42), dp(48))
        b.setBackgroundColor(Color.TRANSPARENT)
        b.setColorFilter(0xFFE8ECF1.toInt())
        b.contentDescription = text
        return b
    }

    private fun buildUi() {
        val root = LinearLayout(this)
        root.orientation = LinearLayout.VERTICAL
        root.setBackgroundColor(0xFF0B0F14.toInt())

        // 地址栏行
        val bar = LinearLayout(this)
        bar.orientation = LinearLayout.HORIZONTAL
        bar.gravity = Gravity.CENTER_VERTICAL
        bar.setPadding(dp(4), dp(4), dp(4), dp(4))
        bar.setBackgroundColor(0xFF10141A.toInt())

        val backBtn = barButton("后退"); backBtn.setImageResource(android.R.drawable.ic_media_previous)
        backBtn.setOnClickListener { if (canGoBack) session.goBack() }
        val fwdBtn = barButton("前进"); fwdBtn.setImageResource(android.R.drawable.ic_media_next)
        fwdBtn.setOnClickListener { if (canGoForward) session.goForward() }

        urlBar = EditText(this)
        urlBar.layoutParams = LinearLayout.LayoutParams(0, dp(40), 1f)
        urlBar.setSingleLine(true)
        urlBar.imeOptions = EditorInfo.IME_ACTION_GO
        urlBar.setTextColor(0xFFF2F5F8.toInt())
        urlBar.setHintTextColor(0xFF8A94A0.toInt())
        urlBar.hint = "搜索或输入网址"
        urlBar.textSize = 15f
        urlBar.setPadding(dp(14), 0, dp(14), 0)
        val pill = GradientDrawable()
        pill.cornerRadius = dp(20).toFloat()
        pill.setColor(0xFF1A212B.toInt())
        pill.setStroke(dp(1), 0xFF2C3644.toInt())
        urlBar.background = pill
        urlBar.setOnEditorActionListener { _, actionId, event ->
            val go = actionId == EditorInfo.IME_ACTION_GO ||
                (event != null && event.keyCode == KeyEvent.KEYCODE_ENTER && event.action == KeyEvent.ACTION_DOWN)
            if (go) { navigateFromBar(); true } else false
        }
        urlBar.setOnFocusChangeListener { _, hasFocus ->
            if (hasFocus) urlBar.selectAll()
        }

        val reloadBtn = barButton("刷新"); reloadBtn.setImageResource(android.R.drawable.ic_menu_rotate)
        reloadBtn.setOnClickListener { session.reload() }

        val menuBtn = barButton("菜单"); menuBtn.setImageResource(android.R.drawable.ic_menu_more)
        menuBtn.setOnClickListener { showMenu(it) }

        bar.addView(backBtn); bar.addView(fwdBtn); bar.addView(urlBar)
        bar.addView(reloadBtn); bar.addView(menuBtn)
        root.addView(bar)

        progress = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal)
        progress.layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(3))
        progress.max = 100
        progress.progress = 0
        progress.visibility = View.GONE
        val pd = progress.progressDrawable
        if (pd != null) {
            // 琥珀色进度条
            progress.progressDrawable = pd
        }
        root.addView(progress)

        val frame = FrameLayout(this)
        frame.layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f
        )
        geckoView = GeckoView(this)
        frame.addView(
            geckoView,
            FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT
            )
        )
        root.addView(frame)

        setContentView(root)
    }

    private fun showMenu(anchor: View) {
        val menu = PopupMenu(this, anchor)
        menu.menu.add(0, 1, 0, "主页")
        menu.menu.add(0, 2, 1, "插件管理")
        menu.menu.add(0, 3, 2, "刷新")
        menu.menu.add(0, 4, 3, "分享网页")
        menu.setOnMenuItemClickListener { item ->
            when (item.itemId) {
                1 -> session.loadUri(HOME_URL)
                2 -> startActivity(Intent(this, ExtensionsActivity::class.java))
                3 -> session.reload()
                4 -> {
                    val share = Intent(Intent.ACTION_SEND)
                    share.type = "text/plain"
                    share.putExtra(Intent.EXTRA_TEXT, urlBar.text.toString())
                    startActivity(Intent.createChooser(share, "分享"))
                }
            }
            true
        }
        menu.show()
    }

    // ---------- 导航 ----------

    private fun navigateFromBar() {
        val raw = urlBar.text.toString().trim()
        if (raw.isEmpty()) return
        val url = normalize(raw)
        session.loadUri(url)
        hideKeyboard()
    }

    private fun normalize(raw: String): String {
        if (raw.startsWith("http://") || raw.startsWith("https://") ||
            raw.startsWith("resource://") || raw.startsWith("file://") || raw.startsWith("about:")
        ) return raw
        val looksLikeHost = !raw.contains(" ") && (raw.contains(".") || raw == "localhost")
        return if (looksLikeHost) {
            "https://$raw"
        } else {
            "https://www.google.com/search?q=" + URLEncoder.encode(raw, "UTF-8")
        }
    }

    private fun hideKeyboard() {
        val imm = getSystemService(INPUT_METHOD_SERVICE) as InputMethodManager
        imm.hideSoftInputFromWindow(urlBar.windowToken, 0)
        urlBar.clearFocus()
    }

    private fun wireDelegates() {
        session.navigationDelegate = object : GeckoSession.NavigationDelegate {
            override fun onLocationChange(
                session: GeckoSession,
                url: String?,
                perms: MutableList<GeckoSession.PermissionDelegate.ContentPermission>,
                hasUserGesture: Boolean
            ) {
                if (url != null && !urlBar.hasFocus()) {
                    urlBar.setText(if (url == HOME_URL) "" else url)
                }
            }

            override fun onLoadError(
                session: GeckoSession,
                uri: String?,
                error: WebRequestError
            ): GeckoResult<String>? {
                // 起始页 resource:// 万一打不开，退回解压到私有目录的 file:// 副本
                if (uri == HOME_URL && !homeFallbackTried) {
                    homeFallbackTried = true
                    val f = extractHome()
                    if (f != null) {
                        session.loadUri("file://" + f.absolutePath)
                        return GeckoResult.fromValue(null)
                    }
                }
                return super.onLoadError(session, uri, error)
            }
        }

        session.historyDelegate = object : GeckoSession.HistoryDelegate {
            override fun onHistoryStateChange(
                session: GeckoSession,
                historyList: GeckoSession.HistoryDelegate.HistoryList
            ) {
                canGoBack = historyList.currentIndex > 0
                canGoForward = historyList.currentIndex < historyList.size - 1
            }
        }

        session.progressDelegate = object : GeckoSession.ProgressDelegate {
            override fun onPageStart(session: GeckoSession, url: String) {
                progress.visibility = View.VISIBLE
                progress.progress = 0
            }

            override fun onPageStop(session: GeckoSession, success: Boolean) {
                progress.visibility = View.GONE
            }

            override fun onProgressChange(session: GeckoSession, progressValue: Int) {
                progress.progress = progressValue
            }
        }
    }

    /** 把起始页资源解压到应用私有目录（resource:// 的兜底路径） */
    private fun extractHome(): File? {
        return try {
            val dir = File(filesDir, "ntp")
            val names = listOf(
                "ntp.html", "ntp.css", "ntp.js", "ntp3d.js",
                "bookmarks.js", "yousj3d.module.js", "bg.jpg"
            )
            for (name in names) {
                val out = File(dir, name)
                if (!out.exists()) {
                    out.parentFile?.mkdirs()
                    assets.open("ntp/$name").use { input ->
                        out.outputStream().use { input.copyTo(it) }
                    }
                }
            }
            File(dir, "ntp.html")
        } catch (e: Exception) {
            null
        }
    }

    override fun onBackPressed() {
        if (canGoBack) session.goBack() else super.onBackPressed()
    }

    override fun onDestroy() {
        try { session.close() } catch (e: Exception) { /* ignore */ }
        super.onDestroy()
    }

    fun toast(msg: String) {
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()
    }
}
