use hidapi::HidApi;

// Define Elgato's Vendor ID as a constant.
const ELGATO_VENDOR_ID: u16 = 0x0fd9;

fn main() {
    println!("Searching for a Stream Deck...");

    // Initialize the HIDAPI. If this fails, it's likely due to a missing
    // system library (like libhidapi-dev on Debian/Ubuntu).
    let api = HidApi::new().expect("Failed to initialize HIDAPI");

    // A flag to track if we found the device.
    let mut deck_found = false;

    // Iterate over all connected HID devices.
    for device_info in api.device_list() {
        // Check if the Vendor ID matches Elgato's.
        if device_info.vendor_id() == ELGATO_VENDOR_ID {
            // Get the product name, providing a default if it's not available.
            let product_name = device_info.product_string().unwrap_or("Unknown Device");
            println!("✅ Success! Found Stream Deck: {}", product_name);

            deck_found = true;
            break; // Exit the loop since we found what we're looking for.
        }
    }

    if !deck_found {
        println!("❌ Failure: No Stream Deck device was found.");
    }
}
