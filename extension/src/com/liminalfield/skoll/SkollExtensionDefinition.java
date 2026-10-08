package com.liminalfield.skoll;

import com.bitwig.extension.api.PlatformType;
import com.bitwig.extension.controller.AutoDetectionMidiPortNamesList;
import com.bitwig.extension.controller.ControllerExtension;
import com.bitwig.extension.controller.ControllerExtensionDefinition;
import com.bitwig.extension.controller.api.ControllerHost;
import java.util.UUID;

public class SkollExtensionDefinition extends ControllerExtensionDefinition {
    // Never change this: Bitwig identifies the installed extension by it.
    private static final UUID ID = UUID.fromString("c47d9300-213b-4fce-ae97-918afc576660");

    @Override
    public String getName() {
        return "Skoll Transport";
    }

    @Override
    public String getAuthor() {
        return "Liminal Field";
    }

    @Override
    public String getVersion() {
        return "0.1.0";
    }

    @Override
    public UUID getId() {
        return ID;
    }

    @Override
    public String getHardwareVendor() {
        return "Liminal Field";
    }

    @Override
    public String getHardwareModel() {
        return "Skoll Transport";
    }

    @Override
    public int getRequiredAPIVersion() {
        // playPositionInSeconds() needs API version 10.
        return 10;
    }

    @Override
    public int getNumMidiInPorts() {
        return 0;
    }

    @Override
    public int getNumMidiOutPorts() {
        return 0;
    }

    @Override
    public void listAutoDetectionMidiPortNames(
            AutoDetectionMidiPortNamesList list, PlatformType platformType) {}

    @Override
    public ControllerExtension createInstance(ControllerHost host) {
        return new SkollExtension(this, host);
    }
}
