import sys
import webbrowser

import pystray
from PIL import Image, ImageDraw, ImageFont

url = 'https://www.pixiv.net/bookmark_new_illust.php'
cleanup = notify = None
sys.stdout = sys.stderr


def create_image(width, height, num, fg_color, bg_color):
    img = Image.new('RGB', (width, height), color=bg_color)
    draw = ImageDraw.Draw(img)
    text = str(num)

    font_size = int(height * 0.8)
    font = ImageFont.truetype('arialbd.ttf', font_size)

    while font.getbbox(text)[2] > width:
        font_size -= 1
        try:
            font = ImageFont.truetype('arialbd.ttf', font_size)
        except IOError:
            break

    bbox = draw.textbbox((0, 0), text, font=font)
    text_width = bbox[2] - bbox[0]
    text_height = bbox[3] - bbox[1]

    x = (width - text_width) / 2
    y = (height - text_height) / 2 - bbox[1]

    draw.text((x, y), text, font=font, fill=fg_color)

    return img


def create_icon_image(num):
    return create_image(64, 64, num, 'white', '#0099ff')


def click(_icon, _item):
    try:
        if webbrowser.open(url):
            print('Opened', url)
        else:
            print('Failed to open', url)
    finally:
        if notify:
            notify()


def quit(icon, _item):
    try:
        print('Quitting...')
        icon.stop()
    finally:
        if cleanup:
            cleanup()


icon = pystray.Icon(
    'QAQ',
    icon=create_icon_image(42),
    menu=pystray.Menu(
        pystray.MenuItem('Go...', click, default=True), pystray.MenuItem('Quit', quit)
    ),
)


def update(num):
    icon.icon = create_icon_image(num)


def start(cleanup_callback=None, notify_callback=None):
    global cleanup, notify
    cleanup = cleanup_callback
    notify = notify_callback
    icon.run_detached()


if __name__ == '__main__':
    icon.run()
