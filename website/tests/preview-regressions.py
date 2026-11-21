"""Run against npm run preview with Python Playwright and Google Chrome installed."""

from playwright.sync_api import expect, sync_playwright

PREVIEW = "http://127.0.0.1:4173/"


def check_fallback(page):
    page.route("**/assets/*.js", lambda route: route.abort())
    page.goto(PREVIEW + "?lang=en")
    expect(page.get_by_role("heading", name="OpenPixiv")).to_be_visible()
    for file, name in [("README.ja.md", "日本語の導入ガイド"), ("README.md", "English installation guide")]:
        guide = page.get_by_role("link", name=name)
        expect(guide).to_be_visible()
        assert file + "#" in guide.get_attribute("href")
    page.unroute("**/assets/*.js")


def check_menu(page):
    toggle = page.locator(".mobile-menu summary")
    toggle.press("Enter")
    page.locator("#mobile-nav a").last.press("Tab")
    expect(page.locator(".release-link")).to_be_focused()
    expect(page.locator("#mobile-nav")).to_be_hidden()
    toggle.press("Enter")
    toggle.press("Shift+Tab")
    expect(page.locator("[data-lang=en]")).to_be_focused()
    expect(page.locator("#mobile-nav")).to_be_hidden()


def check_install_and_navigation(page, lang):
    windows, linux = page.locator("#windows-panel"), page.locator("#linux-panel")
    page.locator("#windows-tab").click()
    expect(windows.locator(".hint")).to_contain_text("opixiv.exe")
    page.locator("#linux-tab").press("Space")
    expect(linux.locator(".linux-requirements")).to_contain_text("glibc 2.39")
    expect(linux.locator("a[href='#cargo-install']")).to_be_visible()
    expect(linux.locator(".hint").last).to_contain_text("opixiv")
    page.locator(".mobile-menu summary").press("Enter")
    names = [nav.get_attribute("aria-label") for nav in page.get_by_role("navigation").all()]
    assert set(names) == ({"メインナビゲーション", "フッターナビゲーション"} if lang == "ja"
                          else {"Main navigation", "Footer navigation"})
    assert len(names) == 2
    page.keyboard.press("Escape")


def check_expansion(page, lang):
    page.locator("[data-scene=outputs]").click()
    for name, scene in [("hero", "hero"), ("scene", "outputs")]:
        player = page.locator("#" + name + "-player")
        base = "./recordings/" + scene + ("-en" if lang == "en" else "")
        for link in player.locator("[data-full]").all():
            expect(link).to_have_attribute("href", base + ".png")
        expect(player.locator("[data-size-label]")).to_contain_text("PNG")
        gif = player.locator("[data-gif]")
        expect(gif).to_be_visible()
        expect(gif).to_have_attribute("href", base + ".gif")
    assert page.evaluate("document.documentElement.scrollWidth <= innerWidth")


with sync_playwright() as playwright:
    browser = playwright.chromium.launch(channel="chrome")
    page = browser.new_page(viewport={"width": 320, "height": 844}, reduced_motion="reduce")
    check_fallback(page)
    for lang in ["ja", "en"]:
        page.goto(PREVIEW + ("?lang=en" if lang == "en" else ""))
        check_menu(page)
        check_install_and_navigation(page, lang)
        check_expansion(page, lang)
    page.goto(PREVIEW)
    page.locator(".hero .primary").click()
    page.locator("[data-lang=en]").click()
    page.go_back()
    expect(page.locator("html")).to_have_attribute("lang", "ja")
    expect(page.locator("#hero-player img")).to_have_attribute("src", "./recordings/hero.png")
    page.go_forward()
    expect(page.locator("html")).to_have_attribute("lang", "en")
    expect(page.locator("#hero-player img")).to_have_attribute("src", "./recordings/hero-en.png")
    browser.close()
    print("Preview regression checks passed.")
