//! Scripted mock turns for every eval case id (the mock-provider side of the
//! CI parity gate). One `match` arm per fixture: evidence tool calls that the
//! sim serves from the fixture's `setup`, an `edit_file` whose content hits
//! gold (fix cases), and a terminal text that states the diagnosis. The
//! invariant test in `eval.rs` enforces: every case id has a real script, fix
//! scripts hit gold and avoid `hallucinated` strings, STOP scripts never
//! touch `edit_file`. Unknown ids get an empty terminal turn (which the
//! invariant test makes a failure, not a silent pass).
use crate::agent::MockTurn;
use crate::provider::{ToolCall, ToolCallFunction};

pub(crate) fn script(case_id: &str) -> Vec<MockTurn> {
    match case_id {
        "wrong-i2c-address-bme280-0x76" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("i2c", r#"{"action":"detect","address":118}"#, 200, 180),
            mturn_call("edit_file", r#"{"path":"bme_simpletest.py","content":"from board import *\nfrom adafruit_bme280 import basic as adafruit_bme280\ni2c = I2C(scl, sda)\nbme = adafruit_bme280.Adafruit_BME280_I2C(i2c, address=0x76)\nprint(bme.humidity)\n"}"#, 280, 260),
            mturn_text("Fixed: BME280 is at 0x76, not the default 0x77. Set address=0x76.", 320, 300),
        ],
        "i2c-wrong-address-0x77" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("i2c", r#"{"action":"scan"}"#, 180, 160),
            mturn_text("i2cdetect is empty even though the header has 3.3 V — SDA/SCL look swapped. This is a wiring fault. STOP coding; rewire the I2C pins.", 240, 220),
        ],
        "i2c-bus-scan-all-addresses" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 100),
            mturn_text("i2cdetect shows every address responding — that means SDA/SCL are shorted to power. This is a hardware/wiring fault. STOP coding; check the wiring and pull-ups before any further I2C op.", 200, 180),
        ],
        "undervoltage-brownout" => vec![
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 130, 110),
            mturn_text("vcgencmd get_throttled shows undervoltage has occurred (bit 16). This is a power-supply problem, not code — use a 5V/3A+ PSU and don't power servos from the 3.3V rail. STOP coding.", 210, 190),
        ],
        "gpio-board-vs-bcm-numbering" => vec![
            mturn_call("edit_file", r#"{"path":"blink.py","content":"from gpiozero import LED\nfrom time import sleep\nled = LED(17)\nwhile True:\n    led.on()\n    sleep(0.5)\n    led.off()\n    sleep(0.5)\n"}"#, 200, 180),
            mturn_text("gpiozero uses BCM numbering. Physical pin 11 is BCM 17; use LED(17).", 260, 240),
        ],
        "pi5-rpigpio-migration-break" => vec![
            mturn_call("edit_file", r#"{"path":"app.py","content":"from gpiozero import LED\nled = LED(17)\nled.on()\n"}"#, 200, 180),
            mturn_text("RPi.GPIO cannot drive Pi 5 GPIO. Rewrote with gpiozero.", 260, 240),
        ],
        "servo-jitter-software-pwm" => vec![
            mturn_call("edit_file", r#"{"path":"servo.py","content":"from gpiozero import Servo\nfrom time import sleep\nservo = Servo(17)\nwhile True:\n    sleep(1)\n"}"#, 200, 180),
            mturn_text("Software PWM jitters under Linux. Use gpiozero Servo (hardware-timed on Pi 5).", 260, 240),
        ],
        "bmp280-vs-bme280-chipid" => vec![
            mturn_call("i2c", r#"{"action":"read","address":118,"register":208,"length":1}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"bme.py","content":"from board import *\nimport adafruit_bmp280\ni2c = I2C(scl, sda)\nbmp = adafruit_bmp280.Adafruit_BMP280_I2C(i2c, address=0x76)\nprint(bmp.pressure)\n"}"#, 200, 180),
            mturn_text("Chip-id 0x58 at 0xD0 is a BMP280, not a BME280. Switched to the BMP280 driver.", 260, 240),
        ],
        "bme280-pressure-unit-conversion" => vec![
            mturn_call("edit_file", r#"{"path":"bme_read.py","content":"def read_pressure(reg_bytes):\n    raw = int.from_bytes(reg_bytes, 'big')\n    return raw * 100  # hPa to Pa\n"}"#, 200, 180),
            mturn_text("Pressure was in hPa; multiply by 100 to report Pa.", 260, 240),
        ],
        "i2c-not-enabled" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"config.txt","content":"arm_64bit=1\ndtparam=i2c_arm=on\n"}"#, 200, 180),
            mturn_text("I2C overlay was off. Added dtparam=i2c_arm=on to workspace config.txt.", 260, 240),
        ],
        "ds18b20-1wire-overlay-missing" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"config.txt","content":"arm_64bit=1\ndtoverlay=w1-gpio\n"}"#, 200, 180),
            mturn_text("1-wire overlay missing. Added dtoverlay=w1-gpio to workspace config.txt.", 260, 240),
        ],
        "iio-scale-misapply" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"adc_read.py","content":"raw = int(open(\"/sys/bus/iio/devices/iio:device0/in_voltage0_raw\").read())\nscale = float(open(\"/sys/bus/iio/devices/iio:device0/in_voltage_scale\").read())\nprint(raw * scale / 1000)\n"}"#, 200, 180),
            mturn_text("Raw IIO counts must be multiplied by in_voltage_scale.", 260, 240),
        ],
        "i2c-device-not-found-timeout" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"mpu.py","content":"from mpu6050 import MPU6050\nsensor = MPU6050(address=0x68)\nprint(sensor.get_accel_data())\n"}"#, 200, 180),
            mturn_text("MPU6050 is at address=0x68, not 0x69.", 260, 240),
        ],
        "chipid-bmp390-vs-bmp280" => vec![
            mturn_call("i2c", r#"{"action":"read","address":118,"register":0,"length":1}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"pressure.py","content":"import board\nimport adafruit_bmp3xx\ni2c = board.I2C()\nsensor = adafruit_bmp3xx.BMP3XX_I2C(i2c)\nprint(sensor.pressure)\n"}"#, 200, 180),
            mturn_text("Chip id 0x50 at register 0x00 is a BMP390. Switched to the BMP3XX driver.", 260, 240),
        ],
        "chipid-mpu6050-vs-icm20948" => vec![
            mturn_call("i2c", r#"{"action":"read","address":105,"register":0,"length":1}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"imu.py","content":"from icm20948 import ICM20948\nsensor = ICM20948(0x69)\nprint(sensor.get_accel_data())\n"}"#, 200, 180),
            mturn_text("WHO_AM_I reads 0xE9 — an ICM-20948. Switched to the ICM20948 library.", 260, 240),
        ],
        "chipid-bme680-vs-bme280" => vec![
            mturn_call("i2c", r#"{"action":"read","address":118,"register":208,"length":1}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"env.py","content":"import board\nimport adafruit_bme680\ni2c = board.I2C()\nsensor = adafruit_bme680.Adafruit_BME680_I2C(i2c)\nprint(sensor.humidity, sensor.gas)\n"}"#, 200, 180),
            mturn_text("Chip id 0x60 at 0xD0 is a BME680. Switched to the BME680 driver.", 260, 240),
        ],
        "adc-voltage-scale-ads1115" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"adc.py","content":"raw = int(open(\"/sys/bus/iio/devices/iio:device0/in_voltage0_raw\").read())\nscale = float(open(\"/sys/bus/iio/devices/iio:device0/in_voltage_scale\").read())\nprint(raw * scale)\n"}"#, 200, 180),
            mturn_text("The ADS1115 channel reports raw counts; multiply by in_voltage_scale (0.000125 V/bit) to get volts.", 260, 240),
        ],
        "adc-voltage-scale-mcp3008" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"mcp.py","content":"raw = int(open(\"/sys/bus/iio/devices/iio:device0/in_voltage0_raw\").read())\nscale = float(open(\"/sys/bus/iio/devices/iio:device0/in_voltage_scale\").read())\nprint(raw * scale)\n"}"#, 200, 180),
            mturn_text("10-bit counts with a 3.3 V reference: multiply by in_voltage_scale, not divide by 1024.", 260, 240),
        ],
        "gpio-active-low-led" => vec![
            mturn_call("edit_file", r#"{"path":"led.py","content":"from gpiozero import LED\nfrom time import sleep\nled = LED(17, active_state=False)\nwhile True:\n    led.on()\n    sleep(0.5)\n    led.off()\n    sleep(0.5)\n"}"#, 200, 180),
            mturn_text("The module is active-low: the pin must go LOW to switch on. Set active_state=False.", 260, 240),
        ],
        "gpio-input-pullup-button" => vec![
            mturn_call("edit_file", r#"{"path":"button.py","content":"from gpiozero import Button\nbutton = Button(27, pull_up=True, bounce_time=0.1)\nbutton.wait_for_press()\nprint(\"pressed\")\n"}"#, 200, 180),
            mturn_text("Button to ground needs the internal pull-up and debouncing: pull_up=True + bounce_time.", 260, 240),
        ],
        "gpio-bcm-vs-wiringpi-pin" => vec![
            mturn_call("edit_file", r#"{"path":"blink.py","content":"from gpiozero import PWMLED\nled = PWMLED(17)\nled.pulse()\n"}"#, 200, 180),
            mturn_text("wiringPi pin 0 is its own virtual scheme, not BCM. On the Pi 5 use gpiozero with BCM numbers: PWMLED(17).", 260, 240),
        ],
        "gpio-pwm-hardware-pin-bcm12" => vec![
            mturn_call("edit_file", r#"{"path":"servo.py","content":"from gpiozero import Servo\nservo = Servo(12)\nservo.mid()\n"}"#, 200, 180),
            mturn_text("Hardware PWM0 on the header is BCM 12. Moved the servo to Servo(12) with the pwm overlay.", 260, 240),
        ],
        "gpio-physical13-vs-bcm27" => vec![
            mturn_call("edit_file", r#"{"path":"sense.py","content":"from gpiozero import Button\nirq = Button(27)\nirq.wait_for_press()\nprint(\"interrupt\")\n"}"#, 200, 180),
            mturn_text("Physical header pin 13 is BCM 27. gpiozero takes BCM numbers: Button(27).", 260, 240),
        ],
        "1wire-overlay-wrong-gpio" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"config.txt","content":"arm_64bit=1\ndtoverlay=w1-gpio,gpiopin=17\n"}"#, 200, 180),
            mturn_text("The w1-gpio overlay defaults to GPIO4 but the data wire is on GPIO17. Set gpiopin=17.", 260, 240),
        ],
        "pwm-overlay-missing" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"config.txt","content":"arm_64bit=1\ndtoverlay=pwm\n"}"#, 200, 180),
            mturn_text("Hardware PWM needs its overlay. Added dtoverlay=pwm to workspace config.txt.", 260, 240),
        ],
        "ds18b20-celsius-rounding" => vec![
            mturn_call("hardware_inventory", r#"{}"#, 110, 0),
            mturn_call("edit_file", r#"{"path":"temp.py","content":"line = open(\"/sys/bus/w1/devices/28-00000a1b2c3d/w1_slave\").read()\nraw = int(line.split(\"t=\")[1])\nprint(round(raw / 1000.0, 2))\n"}"#, 200, 180),
            mturn_text("w1_slave t= is milli-degrees C; integer division truncated it. Use raw / 1000.0.", 260, 240),
        ],
        "temperature-c-to-f-conversion" => vec![
            mturn_call("edit_file", r#"{"path":"weather.py","content":"c = read_celsius()\nf = c * 9 / 5 + 32\nprint(f)\n"}"#, 200, 180),
            mturn_text("C to F is c * 9 / 5 + 32, not c * 2 + 32.", 260, 240),
        ],
        "pressure-hpa-to-inhg-conversion" => vec![
            mturn_call("edit_file", r#"{"path":"display.py","content":"hpa = sensor.pressure\ninhg = hpa * 0.02953\nprint(inhg)\n"}"#, 200, 180),
            mturn_text("The sensor reports hPa; inHg needs hPa * 0.02953.", 260, 240),
        ],
        "iio-buffer-overflow" => vec![
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 130, 110),
            mturn_call("edit_file", r#"{"path":"adc_stream.py","content":"base = \"/sys/bus/iio/devices/iio:device0\"\nwith open(base + \"/buffer/length\", \"w\") as f:\n    f.write(\"128\")\nwith open(base + \"/buffer/watermark\", \"w\") as f:\n    f.write(\"16\")\nwith open(base + \"/buffer/enable\", \"w\") as f:\n    f.write(\"1\")\nwhile True:\n    for raw in read_buffer(base):\n        handle(raw)\n"}"#, 200, 180),
            mturn_text("dmesg shows dropped samples: one-shot reads can't keep up. Enabled the triggered buffer with a watermark so the kernel batches.", 260, 240),
        ],
        "mpu6050-alt-address-0x69" => vec![
            mturn_call("i2c", r#"{"action":"detect","address":104}"#, 120, 0),
            mturn_call("i2c", r#"{"action":"read","address":105,"register":117,"length":1}"#, 200, 180),
            mturn_call("edit_file", r#"{"path":"imu.py","content":"from mpu6050 import MPU6050\nsensor = MPU6050(0x69)\nprint(sensor.get_accel_data())\n"}"#, 240, 220),
            mturn_text("AD0 high moves the IMU to 0x69. Updated the address.", 280, 260),
        ],
        "i2c-read-length-two-bytes" => vec![
            mturn_call("i2c", r#"{"action":"read","address":64,"register":2,"length":2}"#, 120, 0),
            mturn_call("edit_file", r#"{"path":"power.py","content":"import smbus\nbus = smbus.SMBus(1)\nval = bus.read_word_data(0x40, 2)\nprint(val)\n"}"#, 200, 180),
            mturn_text("The current register is 16-bit; a single-byte read returns half the word. Use read_word_data.", 260, 240),
        ],
        "i2c-clock-stretch-timeout" => vec![
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 130, 110),
            mturn_call("edit_file", r#"{"path":"config.txt","content":"arm_64bit=1\ndtparam=i2c_arm=on\ndtparam=i2c_arm_baudrate=10000\n"}"#, 200, 180),
            mturn_text("The controller handles clock stretching poorly at 100 kHz. Dropped the bus to baudrate=10000.", 260, 240),
        ],
        "pi5-pigpio-migration" => vec![
            mturn_call("edit_file", r#"{"path":"motor.py","content":"from gpiozero import AngularServo\nservo = AngularServo(17)\nservo.mid()\n"}"#, 200, 180),
            mturn_text("pigpio cannot drive Pi 5 GPIO. Rewrote with gpiozero AngularServo.", 260, 240),
        ],
        "pi5-gpio-character-device" => vec![
            mturn_call("edit_file", r#"{"path":"gpio_old.py","content":"import gpiod\nchip = gpiod.Chip(\"gpiochip4\")\nline = chip.get_line(17)\nline.request(consumer=\"out\", type=gpiod.LINE_REQ_DIR_OUT)\nline.set_value(1)\n"}"#, 200, 180),
            mturn_text("The legacy sysfs interface is gone on the Pi 5. Rewrote against libgpiod.", 260, 240),
        ],
        "i2c-address-conflict-two-devices" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 180, 160),
            mturn_text("Scan shows 0x76 and dmesg says two slaves acked it with CRC errors — both breakouts are strapped to one address. This is a wiring/hardware fault. STOP coding; move one breakout's address jumper and rewire.", 240, 220),
        ],
        "i2c-pullups-missing" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_text("Empty scan with the bus enabled and the module powered: the bare breakout has no pull-up resistors, so SDA/SCL never settle. This is a wiring fault. STOP coding; add 4.7k pull-ups to 3.3 V.", 200, 180),
        ],
        "overcurrent-polyfuse-trip" => vec![
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 130, 110),
            mturn_text("dmesg shows an over-current trip on the USB port — actuator inrush is tripping the polyfuse. This is a power wiring fault. STOP coding; power the actuator from its own supply.", 210, 190),
        ],
        "undervoltage-under-load-now" => vec![
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 130, 110),
            mturn_text("get_throttled shows the under-voltage bit set NOW (0x1): the supply collapses under motor load. This is a power-supply wiring fault. STOP coding; use a 5 V/5 A PSU and a separate rail for the motor.", 210, 190),
        ],
        "gpio-pin-shorted-to-ground" => vec![
            mturn_call("gpio", r#"{"action":"get","pin":17}"#, 120, 100),
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 180, 160),
            mturn_text("The line reads 0 no matter what the code writes, the wire is warm, and dmesg reports the line stuck — the output is shorted to ground. This is a physical wiring fault. STOP coding; disconnect before the pin circuitry is damaged.", 240, 220),
        ],
        "i2c-sda-stuck-low" => vec![
            mturn_call("i2c", r#"{"action":"scan"}"#, 120, 0),
            mturn_call("telemetry", r#"{"action":"snapshot"}"#, 180, 160),
            mturn_text("Every device vanished and dmesg says SDA is held low with bus recovery failing — the newly added module is clamping the data line. This is a hardware wiring fault. STOP coding; disconnect the new module and check its wiring and VCC.", 240, 220),
        ],
        _ => vec![mturn_text("", 100, 0)],
    }
}

fn mturn_call(name: &str, args: &str, prompt: u64, cached: u64) -> MockTurn {
    MockTurn {
        tool_calls: vec![ToolCall {
            id: format!("{name}-1"),
            kind: "function".into(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }],
        text: String::new(),
        prompt_tokens: prompt,
        completion: 50,
        cached,
    }
}
fn mturn_text(text: &str, prompt: u64, cached: u64) -> MockTurn {
    MockTurn {
        tool_calls: vec![],
        text: text.into(),
        prompt_tokens: prompt,
        completion: 50,
        cached,
    }
}
