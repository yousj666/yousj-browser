package cc.yousj.browser

import android.app.Activity
import android.content.Intent
import android.graphics.Color
import android.net.Uri
import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.widget.Toast
import org.mozilla.geckoview.WebExtension
import org.mozilla.geckoview.WebExtensionController
import java.io.File

/** 插件管理：列出全部插件，启停 / 卸载，从文件安装 .xpi */
class ExtensionsActivity : Activity() {

    private lateinit var container: LinearLayout
    private val pickXpi = 42

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val root = LinearLayout(this)
        root.orientation = LinearLayout.VERTICAL
        root.setBackgroundColor(0xFF0B0F14.toInt())
        root.setPadding(dp(16), dp(16), dp(16), dp(16))

        val title = TextView(this)
        title.text = "插件管理"
        title.textSize = 20f
        title.setTextColor(0xFFF2F5F8.toInt())
        root.addView(title)

        val hint = TextView(this)
        hint.text = "支持 Firefox 插件格式（.xpi）。内置 uBlock Origin 已预装。点「安装插件」选一个 .xpi 文件即可装上；用 Convert-Xpi 转过的普通网页扩展也能试装，调用 Firefox 专属接口的插件可能装不上或功能不全"
        hint.textSize = 13f
        hint.setTextColor(0xFF8A94A0.toInt())
        hint.setPadding(0, dp(6), 0, dp(12))
        root.addView(hint)

        val installBtn = Button(this)
        installBtn.text = "＋ 安装插件（.xpi 文件）"
        installBtn.setOnClickListener {
            val intent = Intent(Intent.ACTION_OPEN_DOCUMENT)
            intent.addCategory(Intent.CATEGORY_OPENABLE)
            intent.type = "*/*"
            startActivityForResult(intent, pickXpi)
        }
        root.addView(installBtn)

        val scroll = ScrollView(this)
        scroll.layoutParams = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f
        )
        container = LinearLayout(this)
        container.orientation = LinearLayout.VERTICAL
        scroll.addView(container)
        root.addView(scroll)

        setContentView(root)
        refresh()
    }

    private fun dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

    private fun controller(): WebExtensionController =
        YousjApp.runtime(application).webExtensionController

    private fun refresh() {
        container.removeAllViews()
        val loading = TextView(this)
        loading.text = "读取中…"
        loading.setTextColor(0xFF8A94A0.toInt())
        container.addView(loading)

        controller().list().accept({ list ->
            runOnUiThread {
                container.removeAllViews()
                if (list == null || list.isEmpty()) {
                    val empty = TextView(this)
                    empty.text = "还没有插件"
                    empty.setTextColor(0xFF8A94A0.toInt())
                    container.addView(empty)
                    return@runOnUiThread
                }
                for (ext in list) addRow(ext)
            }
        }, { e ->
            runOnUiThread {
                container.removeAllViews()
                val err = TextView(this)
                err.text = "读取失败：${e?.message}"
                err.setTextColor(0xFFF28B82.toInt())
                container.addView(err)
            }
        })
    }

    private fun addRow(ext: WebExtension) {
        val row = LinearLayout(this)
        row.orientation = LinearLayout.VERTICAL
        row.setPadding(dp(12), dp(10), dp(12), dp(10))
        row.setBackgroundColor(0xFF10141A.toInt())
        val lp = LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT
        )
        lp.setMargins(0, dp(6), 0, dp(6))
        row.layoutParams = lp

        val meta = ext.metaData
        val name = TextView(this)
        name.text = meta.name ?: ext.id
        name.textSize = 16f
        name.setTextColor(0xFFF2F5F8.toInt())
        row.addView(name)

        val sub = TextView(this)
        val state = if (ext.metaData.enabled) "已启用" else "已停用"
        sub.text = "版本 ${meta.version ?: "-"} · $state"
        sub.textSize = 12f
        sub.setTextColor(0xFF8A94A0.toInt())
        row.addView(sub)

        val btns = LinearLayout(this)
        btns.orientation = LinearLayout.HORIZONTAL
        btns.gravity = Gravity.END

        val toggle = Button(this)
        toggle.text = "停用"
        toggle.setOnClickListener {
            controller().disable(ext, WebExtensionController.EnableSource.USER).accept({
                runOnUiThread { toast("已停用"); refresh() }
            }, { runOnUiThread { toast("操作失败") } })
        }
        val enable = Button(this)
        enable.text = "启用"
        enable.setOnClickListener {
            controller().enable(ext, WebExtensionController.EnableSource.USER).accept({
                runOnUiThread { toast("已启用"); refresh() }
            }, { runOnUiThread { toast("操作失败") } })
        }
        val uninstall = Button(this)
        uninstall.text = "卸载"
        uninstall.setTextColor(0xFFF28B82.toInt())
        uninstall.setOnClickListener {
            controller().uninstall(ext).accept({
                runOnUiThread { toast("已卸载"); refresh() }
            }, { runOnUiThread { toast("卸载失败") } })
        }
        btns.addView(enable); btns.addView(toggle); btns.addView(uninstall)
        row.addView(btns)

        container.addView(row)
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode != pickXpi || resultCode != RESULT_OK) return
        val uri: Uri = data?.data ?: return
        try {
            val dir = File(filesDir, "extensions")
            dir.mkdirs()
            val name = "ext-" + System.currentTimeMillis() + ".xpi"
            val out = File(dir, name)
            contentResolver.openInputStream(uri)?.use { input ->
                out.outputStream().use { input.copyTo(it) }
            } ?: throw IllegalStateException("读不到文件")
            controller().install("file://" + out.absolutePath).accept({ ext ->
                runOnUiThread {
                    toast("已安装：${ext?.metaData?.name ?: "插件"}")
                    refresh()
                }
            }, { e ->
                runOnUiThread { toast("安装失败：${e?.message}") }
            })
        } catch (e: Exception) {
            toast("安装失败：${e.message}")
        }
    }

    private fun toast(msg: String) {
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()
    }
}
